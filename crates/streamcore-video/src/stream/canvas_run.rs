//! The combined canvas on the stream executor (3.9). Rust keeps the policy:
//!
//! - **Group start and stop, all or nothing.** One display capture per member
//!   (`CaptureTarget::Display`, sized to its tile), opened and started as one
//!   group: the start completes when every member answered and fails if any
//!   failed; a failed start stops the members that started. A stop completes
//!   when every running member confirmed; a member whose stop failed stays
//!   running (`StopFailed`), and a stop or start without every answer in 10 s
//!   is `*Unknown` with the captures retained.
//! - **First images.** The start is acknowledged (`Started`, then the command's
//!   `Completed`) only once every member delivered a complete image, within
//!   5 s of the group start, else the stream fails with `stream.firstFrame`.
//!   A member is never shown black: nothing is composed before then, and a
//!   member that goes blank, suspended or stopped later fails the stream with
//!   `stream.memberUnavailable`.
//! - **Re-observation.** The layout is observed before anything native is
//!   allocated, when the first images are in, and every 250 ms
//!   (`CaptureBackend::observe_layout`); any difference, a member's layout
//!   event or a gone member fails the stream with `stream.layoutChanged`.
//! - **Cadence.** A member image marks the canvas dirty; a composite is drawn at
//!   most once per frame interval (a timer covers the rest) and only while
//!   dirty, so a still screen composes nothing. The composite is due for
//!   submit at once: the cadence is its pacer.
//! - **Pool.** `PoolExhausted` (or `OutOfResources`) waits for Rust to release
//!   a composite, not for the next member image: on a still screen nothing
//!   arrives. With no composite of its own to release, it retries after one
//!   frame interval.
//! - **Time.** A composite's H0 is the newest changed member's H0 (H1 as its
//!   H1): an idle member's image can be seconds old. The spread between the
//!   newest and oldest member image is a metric.
//! - **Leases.** Rust holds each member's latest image and at most one more
//!   undelivered in the mailbox (≤ 2, 3.2); composites are the latest plus
//!   those in flight (≤ 1 + 3), from a pool of at least permits + 2 (5).

use super::*;
use crate::canvas::{self, limits};
use crate::controller::CanvasParts;
use std::sync::atomic::Ordering;
use streamcore_media::video_control::{ContentRect, LayoutInfo, SourceRect};

/// Every member's first image must arrive within this of the group start.
pub(super) const FIRST_IMAGE_DEADLINE_NS: u64 = 5_000_000_000;
/// The layout is re-observed this often.
pub(super) const OBSERVE_INTERVAL_NS: u64 = 250_000_000;

struct MemberImage {
    /// Rust's hold on the member's latest surface.
    _lease: SurfaceLease,
    info: SurfaceInfo,
}

struct Member {
    display_id: u32,
    capture: Option<CaptureId>,
    /// Its start was confirmed and no stop since.
    running: bool,
    image: Option<MemberImage>,
    /// A new image since the last composite.
    changed: bool,
}

/// One group operation: the stream's op token, answered once every member's
/// own op has been.
struct GroupOp {
    kind: OpKind,
    token: OpToken,
    pending: Vec<(usize, OpToken)>,
    error: Option<MediaError>,
}

pub(crate) struct CanvasRun {
    layout: CanvasArrangement,
    compositor: Arc<dyn Compositor>,
    tiles: Vec<CanvasTile>,
    members: Vec<Member>,
    group: Option<GroupOp>,
    /// Every member delivered a first image and the start was acknowledged.
    ready: bool,
    dirty: bool,
    /// A compose found the pool exhausted: wait for a composite release.
    pool_blocked: bool,
    /// Executor clock of the last composite.
    last_compose: Option<u64>,
    compose_timer: Option<TimerToken>,
    deadline_timer: Option<TimerToken>,
    observe_timer: Option<TimerToken>,
}

impl CanvasRun {
    pub(super) fn new(parts: CanvasParts, plan: &StreamPlan) -> Self {
        let tiles = canvas::tiles(&parts.layout, plan.encode_width, plan.encode_height);
        let members = parts
            .layout
            .members
            .iter()
            .map(|m| Member {
                display_id: m.display_id,
                capture: None,
                running: false,
                image: None,
                changed: false,
            })
            .collect();
        Self {
            layout: parts.layout,
            compositor: parts.compositor,
            tiles,
            members,
            group: None,
            ready: false,
            dirty: false,
            pool_blocked: false,
            last_compose: None,
            compose_timer: None,
            deadline_timer: None,
            observe_timer: None,
        }
    }

    pub(super) fn layout(&self) -> &CanvasArrangement {
        &self.layout
    }

    fn member_of(&self, capture: CaptureId) -> Option<usize> {
        self.members.iter().position(|m| m.capture == Some(capture))
    }
}

/// A canvas spec and plan (2.5.7, 2.5.8): the layout is valid, the target names
/// its displays in order, the encode size is a canvas of the layout's
/// proportions within the limits, every tile is at least 2 × 2, and the encoder
/// reads the composite (YUV) directly.
pub(super) fn validate(
    spec: &CaptureSpec,
    plan: &StreamPlan,
    display_ids: &[u32],
    layout: &CanvasArrangement,
) -> Result<(), MediaError> {
    let ids: Vec<u32> = layout.members.iter().map(|m| m.display_id).collect();
    if canvas::problem(layout).is_some()
        || ids != display_ids
        || plan.convert != ConvertPath::Identity
        || !plan.encoder_input.is_yuv()
        || (spec.pixel_width, spec.pixel_height) != (plan.encode_width, plan.encode_height)
        || !canvas::valid_canvas_size(layout, plan.encode_width, plan.encode_height)
        || !(1..=8).contains(&spec.queue_depth)
    {
        return Err(MediaError::InvalidSettings);
    }
    Ok(())
}

impl StreamState {
    fn member_token(&mut self) -> OpToken {
        self.next_op += 1;
        OpToken {
            id: self.next_op,
            epoch: self.epoch,
        }
    }

    // -----------------------------------------------------------------------
    // Group lifecycle.

    /// The start command's native part: observe, configure the compositor,
    /// create the encoder, open every member, then start them as one group.
    pub(super) fn start_canvas(&mut self, cx: &Arc<Shared>) {
        if !self.canvas_layout_current(cx) {
            return self.finish_command(cx, Self::rejected(codes::LAYOUT_CHANGED));
        }
        let Some(run) = &self.canvas else {
            return;
        };
        let compositor = run.compositor.clone();
        let plan = CanvasPlan {
            width: self.plan.encode_width,
            height: self.plan.encode_height,
            tiles: run.tiles.clone(),
            color: self.plan.color_out,
            pool_size: limits::COMPOSITE_POOL,
        };
        let member_specs: Vec<CaptureSpec> = run
            .members
            .iter()
            .zip(&run.tiles)
            .map(|(member, tile)| CaptureSpec {
                target: CaptureTarget::Display {
                    display_id: member.display_id,
                },
                pixel_width: tile.rect.width,
                pixel_height: tile.rect.height,
                adapter_luid: self.plan.adapter_luid,
                ..self.spec.clone()
            })
            .collect();
        debug_assert!(is_current(cx.id));
        if let Err(error) = compositor.configure(plan) {
            return self.finish_command(cx, Self::rejected(error.wire_code(codes::OUTPUT)));
        }
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
            shape: std::sync::Mutex::new(Vec::new()),
        });
        cx.set_capture_port(&port);
        let events: Arc<dyn CaptureEvents> = port;
        let mut ids = Vec::with_capacity(member_specs.len());
        for spec in member_specs {
            debug_assert!(is_current(cx.id));
            let opened = capture.open(spec, queue.clone(), events.clone());
            match opened {
                Ok(id) if !ids.contains(&id) => ids.push(id),
                // Two members on one capture would be one image in two tiles.
                Ok(_) | Err(_) => {
                    let error = opened.err().unwrap_or(MediaError::Internal);
                    self.teardown_encoder(cx);
                    return self
                        .finish_command(cx, Self::rejected(error.wire_code(codes::CAPTURE)));
                }
            }
        }
        let token = self.new_op(OpKind::Start);
        let pending: Vec<(usize, OpToken)> =
            (0..ids.len()).map(|i| (i, self.member_token())).collect();
        if let Some(run) = &mut self.canvas {
            for (member, id) in run.members.iter_mut().zip(&ids) {
                member.capture = Some(*id);
                member.running = false;
            }
            run.group = Some(GroupOp {
                kind: OpKind::Start,
                token,
                pending: pending.clone(),
                error: None,
            });
        }
        self.capture = ids.first().copied();
        self.phase = StreamPhase::Starting;
        self.set_timer(cx, self.timings.op_timeout_ns, TimerKind::Op(token));
        for (member, op) in pending {
            debug_assert!(is_current(cx.id));
            capture.start(ids[member], op);
        }
    }

    /// Stop every running member as one group (`begin_stop` cleared the
    /// encoder and the latest frame). With none running the stop completes
    /// at once, still posted.
    pub(super) fn stop_canvas(&mut self, cx: &Arc<Shared>) {
        self.clear_canvas(cx);
        let Some(backends) = &self.backends else {
            return;
        };
        let capture = backends.capture.clone();
        self.phase = StreamPhase::Stopping;
        let token = self.new_op(OpKind::Stop);
        self.set_timer(cx, self.timings.op_timeout_ns, TimerKind::Op(token));
        self.group_call(cx, OpKind::Stop, token, |id, op| capture.stop(id, op));
    }

    /// A rate or capture update for every member, as one group.
    pub(super) fn update_canvas(&mut self, cx: &Arc<Shared>, update: CaptureUpdate) {
        let Some(backends) = &self.backends else {
            return;
        };
        let capture = backends.capture.clone();
        self.phase = StreamPhase::Updating;
        let token = self.new_op(OpKind::Update);
        self.set_timer(cx, self.timings.op_timeout_ns, TimerKind::Op(token));
        self.group_call(cx, OpKind::Update, token, |id, op| {
            capture.update(id, update, op)
        });
    }

    fn group_call(
        &mut self,
        cx: &Arc<Shared>,
        kind: OpKind,
        token: OpToken,
        call: impl Fn(CaptureId, OpToken),
    ) {
        let running: Vec<(usize, CaptureId)> = self
            .canvas
            .as_ref()
            .map(|run| {
                run.members
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.running)
                    .filter_map(|(i, m)| m.capture.map(|id| (i, id)))
                    .collect()
            })
            .unwrap_or_default();
        if running.is_empty() {
            // Nothing native to wait for; the result is still posted.
            cx.post(Event::OpDone {
                op: token,
                error: None,
            });
            return;
        }
        let pending: Vec<(usize, OpToken)> = running
            .iter()
            .map(|(i, _)| (*i, self.member_token()))
            .collect();
        if let Some(run) = &mut self.canvas {
            run.group = Some(GroupOp {
                kind,
                token,
                pending: pending.clone(),
                error: None,
            });
        }
        for ((_, id), (_, op)) in running.iter().zip(pending) {
            debug_assert!(is_current(cx.id));
            call(*id, op);
        }
    }

    /// A member's `op_done`, folded into its group. True when it was one; the
    /// group's own result follows once every member answered (the first error
    /// wins).
    pub(super) fn group_answer(
        &mut self,
        cx: &Arc<Shared>,
        token: OpToken,
        error: Option<MediaError>,
    ) -> bool {
        let Some(run) = &mut self.canvas else {
            return false;
        };
        let Some(group) = &mut run.group else {
            return false;
        };
        let Some(index) = group.pending.iter().position(|(_, op)| *op == token) else {
            return false;
        };
        let (member, _) = group.pending.remove(index);
        match (group.kind, &error) {
            (OpKind::Start, None) => run.members[member].running = true,
            (OpKind::Stop, None) => run.members[member].running = false,
            _ => {}
        }
        if group.error.is_none() {
            group.error = error;
        }
        if group.pending.is_empty()
            && let Some(group) = run.group.take()
        {
            self.on_op_done(cx, group.token, group.error);
        }
        true
    }

    /// Every member stopped: the captures are gone.
    pub(super) fn forget_member_captures(&mut self) {
        if let Some(run) = &mut self.canvas {
            for member in &mut run.members {
                member.capture = None;
                member.running = false;
            }
            run.group = None;
        }
    }

    /// The group started: wait for every member's first image (5 s) and
    /// re-observe the layout every 250 ms.
    pub(super) fn canvas_started(&mut self, cx: &Arc<Shared>) {
        let deadline = self.set_timer(cx, FIRST_IMAGE_DEADLINE_NS, TimerKind::FirstImage);
        let observe = self.set_timer(cx, OBSERVE_INTERVAL_NS, TimerKind::Observe);
        if let Some(run) = &mut self.canvas {
            run.deadline_timer = Some(deadline);
            run.observe_timer = Some(observe);
        }
        self.canvas_ready(cx);
    }

    fn all_images(&self) -> bool {
        self.canvas
            .as_ref()
            .is_some_and(|run| run.members.iter().all(|m| m.image.is_some()))
    }

    /// Acknowledge the start once every member has an image and the layout is
    /// still the reviewed one; then compose the first canvas.
    pub(super) fn canvas_ready(&mut self, cx: &Arc<Shared>) {
        let waiting = self
            .canvas
            .as_ref()
            .is_some_and(|run| !run.ready && run.deadline_timer.is_some());
        if !waiting
            || !self.all_images()
            || self.closing
            || cx.revoked()
            || self.phase != StreamPhase::Running
        {
            return;
        }
        if !self.canvas_layout_current(cx) {
            return self.fail(cx, codes::LAYOUT_CHANGED);
        }
        if let Some(run) = &mut self.canvas {
            run.ready = true;
            run.dirty = true;
            run.deadline_timer = None;
        }
        self.cancel_timers(|kind| *kind == TimerKind::FirstImage);
        self.layout_epoch += 1;
        self.send_canvas_layout(cx);
        self.announce_started(cx);
        self.compose_due(cx);
    }

    /// `layoutInfo` for this epoch (9.4, frames.v4 only): the union of the
    /// members in global units fills the whole picture, upright.
    fn send_canvas_layout(&mut self, cx: &Arc<Shared>) {
        let Some(run) = &self.canvas else {
            return;
        };
        let Some((ux, uy, uw, uh)) = canvas::union(&run.layout) else {
            return;
        };
        let (width, height) = (self.plan.encode_width, self.plan.encode_height);
        let info = LayoutInfo {
            stream_id: 0,
            epoch: u32::try_from(self.layout_epoch).unwrap_or(u32::MAX),
            // A validated layout's coordinates are within ±100 000.
            source: SourceRect {
                x: ux.round() as i32,
                y: uy.round() as i32,
                width: (uw.round() as u32).max(1),
                height: (uh.round() as u32).max(1),
            },
            frame_width: width,
            frame_height: height,
            content: ContentRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            rotation: 0,
        };
        if cx.revoked() {
            return;
        }
        debug_assert!(is_current(cx.id));
        let (_, events) = cx.sink.send_layout(info);
        for event in events {
            self.on_sink_event(cx, event);
        }
    }

    // -----------------------------------------------------------------------
    // Member events.

    pub(super) fn on_member_frame(
        &mut self,
        cx: &Arc<Shared>,
        capture: CaptureId,
        lease: SurfaceLease,
        info: SurfaceInfo,
        status: FrameStatus,
    ) {
        self.stats.frames_received += 1;
        let Some(index) = self.canvas.as_ref().and_then(|run| run.member_of(capture)) else {
            self.stats.stale_events += 1;
            return;
        };
        if self.closing || cx.revoked() || !self.accepts_frames() {
            return;
        }
        let ready = self.canvas.as_ref().is_some_and(|run| run.ready);
        match status {
            FrameStatus::Complete => {}
            // Unchanged: the member keeps its last complete image.
            FrameStatus::Idle | FrameStatus::Started => return,
            // Never show a blank or stopped member's old pixels as current.
            FrameStatus::Blank | FrameStatus::Suspended | FrameStatus::Stopped => {
                if ready {
                    self.fail_soon(cx, codes::MEMBER_UNAVAILABLE);
                }
                return;
            }
        }
        if !valid_surface(&info) {
            return;
        }
        self.stats.canvas_member_frames += 1;
        if let Some(run) = &mut self.canvas {
            let member = &mut run.members[index];
            // The previous image is released first: at most this one and one
            // undelivered in the mailbox are leased (3.2).
            member.image = None;
            member.image = Some(MemberImage {
                _lease: lease,
                info,
            });
            member.changed = true;
            run.dirty = true;
        }
        if ready {
            self.compose_due(cx);
        } else if self.all_images() {
            if self.inline {
                self.post_internal(cx, Internal::CanvasReady);
            } else {
                self.canvas_ready(cx);
            }
        }
    }

    pub(super) fn on_member_gone(
        &mut self,
        cx: &Arc<Shared>,
        capture: CaptureId,
        reason: SourceGone,
    ) {
        let member = self.canvas.as_ref().and_then(|run| run.member_of(capture));
        if member.is_none() || self.closing {
            self.stats.stale_events += 1;
            return;
        }
        self.notify(cx, ObserverJob::SourceGone { reason });
        self.fail(cx, codes::MEMBER_UNAVAILABLE);
    }

    /// A member's size, scale or origin changed: the reviewed layout is gone.
    pub(super) fn on_member_layout(&mut self, cx: &Arc<Shared>, capture: CaptureId) {
        let member = self.canvas.as_ref().and_then(|run| run.member_of(capture));
        if member.is_none() || self.closing {
            self.stats.stale_events += 1;
            return;
        }
        self.fail(cx, codes::LAYOUT_CHANGED);
    }

    /// A failure found during an inline frame drain is reported on the next
    /// wake (an inline drain only admits, composes and submits).
    fn fail_soon(&mut self, cx: &Arc<Shared>, code: &'static str) {
        if self.inline {
            self.post_internal(cx, Internal::Fail(code));
        } else {
            self.fail(cx, code);
        }
    }

    /// True while every member is observed exactly as reviewed.
    fn canvas_layout_current(&self, cx: &Shared) -> bool {
        let (Some(run), Some(backends)) = (&self.canvas, &self.backends) else {
            return false;
        };
        let capture = backends.capture.clone();
        run.layout.members.iter().all(|member| {
            debug_assert!(is_current(cx.id));
            capture
                .observe_layout(member.display_id)
                .is_some_and(|observed| canvas::observed_matches(member, &observed))
        })
    }

    // -----------------------------------------------------------------------
    // Timers.

    pub(super) fn on_canvas_timer(&mut self, cx: &Arc<Shared>, token: TimerToken, kind: TimerKind) {
        let Some(run) = &mut self.canvas else {
            return;
        };
        match kind {
            TimerKind::FirstImage if run.deadline_timer == Some(token) => {
                run.deadline_timer = None;
                if !run.ready {
                    self.fail(cx, codes::FIRST_FRAME);
                }
            }
            TimerKind::Observe if run.observe_timer == Some(token) => {
                run.observe_timer = None;
                let live = matches!(
                    self.phase,
                    StreamPhase::Running | StreamPhase::Updating | StreamPhase::UpdateUnknown
                );
                if !live || self.closing || cx.revoked() {
                    return;
                }
                if !self.canvas_layout_current(cx) {
                    return self.fail(cx, codes::LAYOUT_CHANGED);
                }
                let next = self.set_timer(cx, OBSERVE_INTERVAL_NS, TimerKind::Observe);
                if let Some(run) = &mut self.canvas {
                    run.observe_timer = Some(next);
                }
            }
            TimerKind::Compose if run.compose_timer == Some(token) => {
                run.compose_timer = None;
                self.compose_due(cx);
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Composition.

    pub(super) fn on_pool_released(&mut self, cx: &Arc<Shared>) {
        if let Some(run) = &mut self.canvas
            && run.pool_blocked
        {
            run.pool_blocked = false;
            self.compose_due(cx);
        }
    }

    /// Compose now when the canvas changed and the cadence allows, else arm
    /// the compose timer for when it does.
    fn compose_due(&mut self, cx: &Arc<Shared>) {
        let Some(run) = &self.canvas else {
            return;
        };
        if !run.ready
            || !run.dirty
            || run.pool_blocked
            || self.closing
            || cx.revoked()
            || !self.accepts_frames()
        {
            return;
        }
        let now = cx.clock.now_ns();
        let interval = interval_ns(self.plan.fps);
        // The display refresh's half (or a quarter interval) of slack, as the
        // pacer's admission allows (6.3).
        let slack = self
            .plan
            .refresh_interval_ns
            .map_or(interval / 4, |refresh| (refresh / 2).min(interval / 2));
        if let Some(last) = run.last_compose {
            let due = last.saturating_add(interval).saturating_sub(slack);
            if now < due {
                if run.compose_timer.is_none() {
                    let at = last.saturating_add(interval);
                    let token = self.set_timer(cx, at.saturating_sub(now), TimerKind::Compose);
                    if let Some(run) = &mut self.canvas {
                        run.compose_timer = Some(token);
                    }
                    if self.inline {
                        self.post_internal(cx, Internal::Arm);
                    }
                }
                return;
            }
        }
        self.compose(cx);
    }

    fn compose(&mut self, cx: &Arc<Shared>) {
        if self
            .canvas
            .as_ref()
            .is_some_and(|run| run.compose_timer.is_some())
        {
            self.cancel_timers(|kind| *kind == TimerKind::Compose);
        }
        let Some(run) = &mut self.canvas else {
            return;
        };
        run.compose_timer = None;
        let compositor = run.compositor.clone();
        let mut surfaces = Vec::with_capacity(run.members.len());
        let mut h0: Option<u64> = None;
        let mut h1: u64 = 0;
        let (mut oldest, mut newest) = (u64::MAX, 0u64);
        for (index, member) in run.members.iter().enumerate() {
            let Some(image) = &member.image else {
                // Never composed with a member missing (it would be black).
                return;
            };
            surfaces.push(MemberSurface {
                member: index as u8,
                surface: image.info.id,
            });
            let time = image
                .info
                .source_present_ns
                .unwrap_or(image.info.capture_ns);
            oldest = oldest.min(time);
            newest = newest.max(time);
            if member.changed {
                h0 = h0.max(image.info.source_present_ns);
                h1 = h1.max(image.info.capture_ns);
            }
        }
        debug_assert!(is_current(cx.id));
        let result = compositor.compose(surfaces);
        match result {
            Ok(ComposeResult::Composed { surface }) => {
                let lease = SurfaceLease::new(
                    surface.id,
                    Releaser::Compositor(Arc::downgrade(&compositor), self.executor.clone()),
                );
                cx.composites_out.fetch_add(1, Ordering::AcqRel);
                if !valid_surface(&surface)
                    || (surface.width, surface.height)
                        != (self.plan.encode_width, self.plan.encode_height)
                {
                    drop(lease);
                    return self.fail_soon(cx, codes::OUTPUT);
                }
                let now = cx.clock.now_ns();
                if let Some(run) = &mut self.canvas {
                    run.dirty = false;
                    run.last_compose = Some(now);
                    for member in &mut run.members {
                        member.changed = false;
                    }
                }
                let spread_us = newest.saturating_sub(oldest) / 1_000;
                self.stats.canvas_composed += 1;
                self.stats.canvas_age_spread_us = spread_us;
                self.stats.canvas_age_spread_max_us =
                    self.stats.canvas_age_spread_max_us.max(spread_us);
                let info = SurfaceInfo {
                    source_present_ns: h0,
                    capture_ns: if h1 == 0 { surface.capture_ns } else { h1 },
                    ..surface
                };
                self.accept_image(cx, lease, info, false);
            }
            Ok(ComposeResult::PoolExhausted) | Err(MediaError::OutOfResources) => {
                self.stats.canvas_pool_exhausted += 1;
                cx.pool_waiting.store(true, Ordering::Release);
                if let Some(run) = &mut self.canvas {
                    run.pool_blocked = true;
                }
                // Nothing of Rust's to release: no release would come.
                if cx.composites_out.load(Ordering::Acquire) == 0
                    && cx.pool_waiting.swap(false, Ordering::AcqRel)
                {
                    let delay = interval_ns(self.plan.fps);
                    let token = self.set_timer(cx, delay, TimerKind::Compose);
                    if let Some(run) = &mut self.canvas {
                        run.pool_blocked = false;
                        run.compose_timer = Some(token);
                    }
                    if self.inline {
                        self.post_internal(cx, Internal::Arm);
                    }
                }
            }
            Err(error) => self.fail_soon(cx, error.wire_code(codes::OUTPUT)),
        }
    }

    // -----------------------------------------------------------------------
    // Clearing.

    /// Release every member image and stop composing (a stop, failure, close
    /// or revocation): every lease is cleared before the backend's stop.
    pub(super) fn clear_canvas(&mut self, cx: &Shared) {
        let Some(run) = &mut self.canvas else {
            return;
        };
        for member in &mut run.members {
            member.image = None;
            member.changed = false;
        }
        run.ready = false;
        run.dirty = false;
        run.pool_blocked = false;
        run.last_compose = None;
        run.compose_timer = None;
        run.deadline_timer = None;
        run.observe_timer = None;
        cx.pool_waiting.store(false, Ordering::Release);
        self.cancel_timers(|kind| {
            matches!(
                kind,
                TimerKind::FirstImage | TimerKind::Observe | TimerKind::Compose
            )
        });
    }

    /// A new start: no image, no group.
    pub(super) fn canvas_reset(&mut self) {
        if let Some(run) = &mut self.canvas {
            for member in &mut run.members {
                member.image = None;
                member.changed = false;
            }
            run.group = None;
            run.ready = false;
            run.dirty = false;
            run.pool_blocked = false;
            run.last_compose = None;
            run.compose_timer = None;
            run.deadline_timer = None;
            run.observe_timer = None;
        }
        self.cancel_timers(|kind| {
            matches!(
                kind,
                TimerKind::FirstImage | TimerKind::Observe | TimerKind::Compose
            )
        });
    }
}
