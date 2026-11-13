//! The combined canvas (3.9): layout policy, the group lifecycle, first
//! images, re-observation, cadence, the composite pool, time and lease caps.

mod support;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use streamcore_video::*;
use support::*;

// ---------------------------------------------------------------------------
// Fakes.

/// One display capture per member: ids are `100 + display`.
struct FakeMembers {
    log: Arc<Log>,
    events: Mutex<Option<Arc<dyn CaptureEvents>>>,
    opened: Mutex<Vec<(CaptureId, CaptureSpec)>>,
    ops: Mutex<Vec<(Op, CaptureId, OpToken)>>,
    /// Surface → its capture, while leased to Rust.
    live: Mutex<HashMap<SurfaceId, CaptureId>>,
    double_release: AtomicUsize,
    peak_member_leases: AtomicUsize,
    next_surface: AtomicU64,
    /// What `observe_layout` reports per display; absent = as reviewed.
    observed: Mutex<HashMap<u32, Option<DisplayLayout>>>,
    fail_open: Mutex<Option<u32>>,
}

fn capture_of(display: u32) -> CaptureId {
    100 + u64::from(display)
}

impl FakeMembers {
    fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            events: Mutex::new(None),
            opened: Mutex::new(Vec::new()),
            ops: Mutex::new(Vec::new()),
            live: Mutex::new(HashMap::new()),
            double_release: AtomicUsize::new(0),
            peak_member_leases: AtomicUsize::new(0),
            next_surface: AtomicU64::new(1),
            observed: Mutex::new(HashMap::new()),
            fail_open: Mutex::new(None),
        })
    }

    fn note_peak(&self) {
        let live = lock(&self.live);
        let mut per: HashMap<CaptureId, usize> = HashMap::new();
        for capture in live.values() {
            *per.entry(*capture).or_default() += 1;
        }
        let peak = per.values().copied().max().unwrap_or(0);
        self.peak_member_leases.fetch_max(peak, Ordering::SeqCst);
    }

    /// A member image: H0 and H1 in ns.
    fn emit(&self, display: u32, h0: Option<u64>, h1: u64) -> SurfaceId {
        self.emit_status(display, h0, h1, FrameStatus::Complete)
    }

    fn emit_status(
        &self,
        display: u32,
        h0: Option<u64>,
        h1: u64,
        status: FrameStatus,
    ) -> SurfaceId {
        let id = self.next_surface.fetch_add(1, Ordering::SeqCst);
        let capture = capture_of(display);
        lock(&self.live).insert(id, capture);
        let info = SurfaceInfo {
            id,
            kind: SurfaceKind::IoSurface,
            format: PixelFormat::Bgra,
            width: 800,
            height: 600,
            color: srgb(),
            capture_ns: h1,
            source_present_ns: h0,
            dirty: None,
            rotation: Rotation::None,
            fence: GpuFence::None,
        };
        let events = lock(&self.events).clone();
        match events {
            Some(events) => events.frame(capture, None, info, status),
            None => self.release(id),
        }
        self.note_peak();
        id
    }

    fn ops(&self, op: Op) -> Vec<(CaptureId, OpToken)> {
        lock(&self.ops)
            .iter()
            .filter(|(o, _, _)| *o == op)
            .map(|(_, c, t)| (*c, *t))
            .collect()
    }

    /// Answer the newest `op` of `display`'s capture.
    fn answer(&self, op: Op, display: u32, error: Option<MediaError>) {
        let token = self
            .ops(op)
            .into_iter()
            .rev()
            .find(|(c, _)| *c == capture_of(display))
            .map(|(_, t)| t)
            .expect("no such op");
        let events = lock(&self.events).clone().expect("not open");
        events.op_done(token, error);
    }

    fn leases(&self) -> usize {
        lock(&self.live).len()
    }
}

impl CaptureBackend for FakeMembers {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            target_kinds: vec![CaptureTargetKind::Display],
            window_fit: false,
            live_region_move: false,
            cursor_modes: vec![CursorMode::Composited],
            surface_kinds: vec![SurfaceKind::IoSurface],
            pool_depth: 5,
            virtual_displays: false,
            region_in_surface: true,
        }
    }
    fn enumerate(&self) -> Result<SourceCatalog, MediaError> {
        Ok(SourceCatalog {
            displays: vec![],
            windows: vec![],
        })
    }
    fn observe_layout(&self, display: u32) -> Option<DisplayLayout> {
        let _call = self.log.outbound();
        if let Some(observed) = lock(&self.observed).get(&display) {
            return observed.clone();
        }
        layout3()
            .members
            .iter()
            .find(|m| m.display_id == display)
            .map(|m| DisplayLayout {
                display_id: display,
                pixel_width: (m.width * m.scale) as u32,
                pixel_height: (m.height * m.scale) as u32,
                scale: m.scale,
                origin_x: m.x as i32,
                origin_y: m.y as i32,
            })
    }
    fn open(
        &self,
        spec: CaptureSpec,
        _queue: Arc<dyn SerialQueue>,
        events: Arc<dyn CaptureEvents>,
    ) -> Result<CaptureId, MediaError> {
        let _call = self.log.outbound();
        let CaptureTarget::Display { display_id } = spec.target else {
            panic!("a member capture is a display capture");
        };
        if *lock(&self.fail_open) == Some(display_id) {
            return Err(MediaError::PermissionDenied);
        }
        *lock(&self.events) = Some(events);
        let id = capture_of(display_id);
        lock(&self.opened).push((id, spec));
        Ok(id)
    }
    fn start(&self, id: CaptureId, op: OpToken) {
        let _call = self.log.outbound();
        lock(&self.ops).push((Op::Start, id, op));
    }
    fn update(&self, id: CaptureId, _update: CaptureUpdate, op: OpToken) {
        let _call = self.log.outbound();
        lock(&self.ops).push((Op::Update, id, op));
    }
    fn stop(&self, id: CaptureId, op: OpToken) {
        let _call = self.log.outbound();
        assert!(
            !lock(&self.live).values().any(|c| *c == id),
            "a member's leases must be cleared before its stop"
        );
        lock(&self.ops).push((Op::Stop, id, op));
    }
    fn release(&self, surface: SurfaceId) {
        if lock(&self.live).remove(&surface).is_none() {
            self.double_release.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct FakeCompositor {
    log: Arc<Log>,
    plans: Mutex<Vec<CanvasPlan>>,
    composes: Mutex<Vec<Vec<MemberSurface>>>,
    live: Mutex<HashSet<SurfaceId>>,
    peak_live: AtomicUsize,
    double_release: AtomicUsize,
    /// Report `PoolExhausted` this many more times.
    exhaust: AtomicUsize,
    next: AtomicU64,
}

impl FakeCompositor {
    fn new(log: Arc<Log>) -> Arc<Self> {
        Arc::new(Self {
            log,
            plans: Mutex::new(Vec::new()),
            composes: Mutex::new(Vec::new()),
            live: Mutex::new(HashSet::new()),
            peak_live: AtomicUsize::new(0),
            double_release: AtomicUsize::new(0),
            exhaust: AtomicUsize::new(0),
            next: AtomicU64::new(1_000_000),
        })
    }
    fn composes(&self) -> usize {
        lock(&self.composes).len()
    }
    fn live(&self) -> usize {
        lock(&self.live).len()
    }
}

impl Compositor for FakeCompositor {
    fn configure(&self, plan: CanvasPlan) -> Result<(), MediaError> {
        let _call = self.log.outbound();
        lock(&self.plans).push(plan);
        Ok(())
    }
    fn compose(&self, members: Vec<MemberSurface>) -> Result<ComposeResult, MediaError> {
        let _call = self.log.outbound();
        lock(&self.composes).push(members);
        let plan = lock(&self.plans).last().cloned().expect("configured");
        if self
            .exhaust
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
            || lock(&self.live).len() >= usize::from(plan.pool_size)
        {
            return Ok(ComposeResult::PoolExhausted);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut live = lock(&self.live);
        live.insert(id);
        self.peak_live.fetch_max(live.len(), Ordering::SeqCst);
        Ok(ComposeResult::Composed {
            surface: SurfaceInfo {
                id,
                kind: SurfaceKind::IoSurface,
                format: PixelFormat::Nv12,
                width: plan.width,
                height: plan.height,
                color: plan.color,
                capture_ns: 0,
                source_present_ns: None,
                dirty: None,
                rotation: Rotation::None,
                fence: GpuFence::MtlSharedEvent {
                    handle: 0,
                    value: id,
                },
            },
        })
    }
    fn release(&self, surface: SurfaceId) {
        if !lock(&self.live).remove(&surface) {
            self.double_release.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Layouts and harness.

fn member(display_id: u32, x: f64, y: f64, width: f64, height: f64, scale: f64) -> CanvasMember {
    CanvasMember {
        display_id,
        x,
        y,
        width,
        height,
        scale,
        rotation: Rotation::None,
    }
}

/// Three displays in an L: a gap at the bottom right.
fn layout3() -> CanvasLayout {
    CanvasLayout {
        members: vec![
            member(1, 0.0, 0.0, 800.0, 600.0, 1.0),
            member(2, 800.0, 0.0, 800.0, 600.0, 1.0),
            member(3, 0.0, 600.0, 800.0, 600.0, 1.0),
        ],
    }
}

fn canvas_spec(layout: &CanvasLayout, size: CanvasSize) -> CaptureSpec {
    CaptureSpec {
        target: CaptureTarget::Canvas {
            display_ids: layout.members.iter().map(|m| m.display_id).collect(),
        },
        pixel_width: size.width,
        pixel_height: size.height,
        format: PixelFormat::Bgra,
        queue_depth: 4,
        ..spec()
    }
}

fn canvas_plan(size: CanvasSize) -> StreamPlan {
    StreamPlan {
        encode_width: size.width,
        encode_height: size.height,
        ..plan()
    }
}

struct Canvas {
    fakes: Fakes,
    members: Arc<FakeMembers>,
    compositor: Arc<FakeCompositor>,
    clock: Arc<ManualClock>,
    queue: Arc<ManualQueue>,
    controller: Arc<StreamController>,
    displays: Vec<u32>,
}

impl Canvas {
    fn new() -> Self {
        Self::with(layout3(), |_| {})
    }

    fn with(layout: CanvasLayout, adjust: impl FnOnce(&mut StreamParts)) -> Self {
        let fakes = Fakes::new();
        let members = FakeMembers::new(fakes.log.clone());
        let compositor = FakeCompositor::new(fakes.log.clone());
        let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
        let port = StreamPort::new();
        let queue = ManualQueue::new(port.clone(), clock.clone());
        let size = canvas_size(layout.clone(), 1600).expect("a canvas");
        let mut parts = fakes.parts(port, queue.clone(), canvas_plan(size));
        parts.spec = canvas_spec(&layout, size);
        parts.capture = members.clone();
        adjust(&mut parts);
        let displays = layout.members.iter().map(|m| m.display_id).collect();
        let controller = StreamController::canvas_with_clock(
            parts,
            CanvasParts {
                layout,
                compositor: compositor.clone(),
            },
            clock.clone(),
        )
        .expect("a valid canvas");
        Self {
            fakes,
            members,
            compositor,
            clock,
            queue,
            controller,
            displays,
        }
    }

    fn pump(&self) {
        self.queue.pump();
    }

    fn advance_ms(&self, ms: u64) {
        self.queue.advance(ms * MS);
    }

    /// Start the group and confirm every member's start.
    fn start_group(&self) -> OpToken {
        let op = self.controller.start();
        self.pump();
        for display in &self.displays {
            self.members.answer(Op::Start, *display, None);
        }
        self.pump();
        op
    }

    /// One image from every member at `h0_ms`.
    fn images(&self, h0_ms: u64) {
        for display in &self.displays {
            self.members
                .emit(*display, Some(h0_ms * MS), h0_ms * MS + 100);
        }
        self.pump();
    }

    /// Started, first images in, acknowledged.
    fn running(&self) -> OpToken {
        let op = self.start_group();
        self.images(1_000);
        assert_eq!(self.fakes.observer.results(op), vec![OpOutcome::Completed]);
        op
    }

    fn encoder(&self) -> Arc<FakeEncoder> {
        self.fakes.factory.last()
    }

    fn close(&self) {
        self.controller.begin_close();
        self.pump();
        for display in &self.displays {
            if self
                .members
                .ops(Op::Stop)
                .iter()
                .any(|(c, _)| *c == capture_of(*display))
            {
                self.members.answer(Op::Stop, *display, None);
            }
        }
        self.pump();
        assert_eq!(self.controller.close_outcome(), Some(CloseOutcome::Closed));
    }

    fn assert_clean(&self) {
        assert_eq!(self.members.leases(), 0, "a member surface never released");
        assert_eq!(self.members.double_release.load(Ordering::SeqCst), 0);
        assert_eq!(self.compositor.live(), 0, "a composite never released");
        assert_eq!(self.compositor.double_release.load(Ordering::SeqCst), 0);
        assert!(self.members.peak_member_leases.load(Ordering::SeqCst) <= 2);
        self.fakes.assert_clean();
    }
}

// ---------------------------------------------------------------------------
// Layout policy.

#[test]
fn layout_validation_mirrors_the_limits() {
    assert!(canvas_validate(layout3()).is_ok());
    let one = CanvasLayout {
        members: vec![member(1, 0.0, 0.0, 800.0, 600.0, 1.0)],
    };
    assert_eq!(canvas_validate(one), Err(MediaError::InvalidSettings));
    let nine = CanvasLayout {
        members: (0..9)
            .map(|i| member(i, f64::from(i) * 100.0, 0.0, 100.0, 100.0, 1.0))
            .collect(),
    };
    assert!(canvas_validate(nine).is_err());
    let eight = CanvasLayout {
        members: (0..8)
            .map(|i| member(i, f64::from(i) * 100.0, 0.0, 100.0, 100.0, 1.0))
            .collect(),
    };
    assert!(canvas_validate(eight).is_ok());
    let mut duplicate = layout3();
    duplicate.members[2].display_id = 1;
    assert!(canvas_validate(duplicate).is_err());
    let mut overlap = layout3();
    overlap.members[1].x = 799.0;
    assert!(canvas_validate(overlap).is_err());
    // Touching edges do not overlap.
    let mut touching = layout3();
    touching.members[1].x = 800.0;
    assert!(canvas_validate(touching).is_ok());
    for bad in [
        member(2, f64::NAN, 0.0, 800.0, 600.0, 1.0),
        member(2, 100_001.0, 0.0, 800.0, 600.0, 1.0),
        member(2, 900.0, 0.0, 0.0, 600.0, 1.0),
        member(2, 900.0, 0.0, 16_385.0, 600.0, 1.0),
        member(2, 900.0, 0.0, 800.0, 600.0, 0.0),
        member(2, 900.0, 0.0, 800.0, 600.0, 8.5),
    ] {
        let mut layout = layout3();
        layout.members[1] = bad;
        assert!(canvas_validate(layout).is_err(), "{bad:?}");
    }
    let wide = CanvasLayout {
        members: vec![
            member(1, -60_000.0, 0.0, 100.0, 100.0, 1.0),
            member(2, 60_000.0, 0.0, 100.0, 100.0, 1.0),
        ],
    };
    assert!(canvas_validate(wide).is_err(), "a union over 65 536");
}

#[test]
fn canvas_size_keeps_one_scale_within_the_limits() {
    // Mixed densities: the densest member's scale is the ceiling.
    let mixed = CanvasLayout {
        members: vec![
            member(1, 0.0, 0.0, 1512.0, 982.0, 2.0),
            member(2, 1512.0, 0.0, 2560.0, 1440.0, 1.0),
        ],
    };
    let size = canvas_size(mixed.clone(), 8192).unwrap();
    assert!(size.width <= 8192 && size.width.is_multiple_of(2) && size.height.is_multiple_of(2));
    assert!(u64::from(size.width) * u64::from(size.height) <= 7680 * 4320);
    let union = (1512.0 + 2560.0) / 1440.0;
    assert!((f64::from(size.width) / f64::from(size.height) - union).abs() < 0.01);
    // A small maximum edge.
    let small = canvas_size(mixed.clone(), 512).unwrap();
    assert!(small.width <= 512);
    assert_eq!(canvas_size(mixed, 1), None);
    // At scale 1 a 1600 × 1200 union is its own size.
    assert_eq!(
        canvas_size(layout3(), 4096),
        Some(CanvasSize {
            width: 1600,
            height: 1200
        })
    );
}

#[test]
fn tiles_share_edges_and_cover_each_member() {
    let tiles = canvas_tiles(layout3(), 1600, 1200);
    let rects: Vec<_> = tiles.iter().map(|t| (t.member, t.rect)).collect();
    assert_eq!(
        rects,
        vec![
            (
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600
                }
            ),
            (
                1,
                Rect {
                    x: 800,
                    y: 0,
                    width: 800,
                    height: 600
                }
            ),
            (
                2,
                Rect {
                    x: 0,
                    y: 600,
                    width: 800,
                    height: 600
                }
            ),
        ]
    );
    // An odd scale: adjacent edges are rounded once, so they meet exactly.
    let tiles = canvas_tiles(layout3(), 1001, 751);
    assert_eq!(tiles[0].rect.x + tiles[0].rect.width, tiles[1].rect.x);
    assert_eq!(tiles[0].rect.y + tiles[0].rect.height, tiles[2].rect.y);
    assert_eq!(tiles[1].rect.x + tiles[1].rect.width, 1001);
}

#[test]
fn input_maps_into_members_and_never_into_a_gap() {
    let layout = layout3();
    // The middle of display 2.
    let point = canvas_input_target(layout.clone(), 0.75, 0.25).unwrap();
    assert_eq!((point.member, point.display_id), (1, 2));
    assert!((point.x - 1200.0).abs() < 1e-9 && (point.y - 300.0).abs() < 1e-9);
    // The gap (bottom right) and outside the canvas.
    assert_eq!(canvas_input_target(layout.clone(), 0.75, 0.75), None);
    assert_eq!(canvas_input_target(layout.clone(), 1.5, 0.5), None);
    assert_eq!(canvas_input_target(layout.clone(), f64::NAN, 0.5), None);
    // A shared edge belongs to the display that starts there.
    let edge = canvas_input_target(layout.clone(), 0.5, 0.25).unwrap();
    assert_eq!(edge.display_id, 2);
    // The far edge stays one native pixel inside.
    let corner = canvas_input_target(layout.clone(), 1.0, 0.0).unwrap();
    assert_eq!(corner.display_id, 2);
    assert!((corner.x - 1599.0).abs() < 1e-9);
    // A cursor in the gap is moved onto the nearest member.
    let nearest = canvas_nearest_member_point(layout, 0.9, 0.9);
    assert!(nearest.x <= 1.0 && (nearest.y - 0.5).abs() < 1e-9 || (nearest.x - 0.5).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Construction.

#[test]
fn a_canvas_without_a_compositor_is_unsupported() {
    let fakes = Fakes::new();
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    let port = StreamPort::new();
    let queue = ManualQueue::new(port.clone(), clock.clone());
    let size = canvas_size(layout3(), 1600).unwrap();
    let mut parts = fakes.parts(port, queue, canvas_plan(size));
    parts.spec = canvas_spec(&layout3(), size);
    assert_eq!(
        StreamController::with_clock(parts, clock).err(),
        Some(MediaError::Unsupported)
    );
}

#[test]
fn a_canvas_is_validated_before_anything_native() {
    let invalid = |adjust: &dyn Fn(&mut StreamParts, &mut CanvasLayout)| {
        let fakes = Fakes::new();
        let compositor = FakeCompositor::new(fakes.log.clone());
        let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
        let port = StreamPort::new();
        let queue = ManualQueue::new(port.clone(), clock.clone());
        let mut layout = layout3();
        let size = canvas_size(layout.clone(), 1600).unwrap();
        let mut parts = fakes.parts(port, queue, canvas_plan(size));
        parts.spec = canvas_spec(&layout, size);
        adjust(&mut parts, &mut layout);
        let result = StreamController::canvas_with_clock(
            parts,
            CanvasParts {
                layout,
                compositor: compositor.clone(),
            },
            clock,
        );
        assert!(lock(&compositor.plans).is_empty());
        result.err()
    };
    assert_eq!(invalid(&|_, _| {}), None);
    // The target names other displays, or another order.
    assert!(
        invalid(&|parts, _| {
            parts.spec.target = CaptureTarget::Canvas {
                display_ids: vec![2, 1, 3],
            }
        })
        .is_some()
    );
    // A size of other proportions, or over the limits.
    assert!(
        invalid(&|parts, _| {
            parts.spec.pixel_width = 1200;
            parts.plan.encode_width = 1200;
        })
        .is_some()
    );
    // The encoder must read the composite directly.
    assert!(invalid(&|parts, _| parts.plan.convert = ConvertPath::Convert).is_some());
    assert!(invalid(&|parts, _| parts.plan.encoder_input = PixelFormat::Bgra).is_some());
    // An invalid layout.
    assert!(invalid(&|_, layout| layout.members[1].x = 10.0).is_some());
    // A layout with a non-canvas target.
    assert!(
        invalid(&|parts, _| parts.spec.target = CaptureTarget::Display { display_id: 1 }).is_some()
    );
}

// ---------------------------------------------------------------------------
// Group lifecycle.

#[test]
fn members_open_at_their_tile_size_and_start_as_one_group() {
    let s = Canvas::new();
    let op = s.controller.start();
    s.pump();
    let opened = lock(&s.members.opened).clone();
    assert_eq!(opened.len(), 3);
    for (id, spec) in &opened {
        assert_eq!((spec.pixel_width, spec.pixel_height), (800, 600), "{id}");
        assert_eq!(spec.queue_depth, 4);
        assert_eq!(spec.format, PixelFormat::Bgra);
    }
    let plan = lock(&s.compositor.plans)[0].clone();
    assert_eq!((plan.width, plan.height), (1600, 1200));
    assert!(plan.pool_size >= 3 + 2, "a pool of at least permits + 2");
    assert_eq!(plan.tiles.len(), 3);
    // Two of three answered: nothing yet.
    s.members.answer(Op::Start, 1, None);
    s.members.answer(Op::Start, 2, None);
    s.pump();
    assert_eq!(s.controller.phase(), StreamPhase::Starting);
    s.members.answer(Op::Start, 3, None);
    s.pump();
    assert_eq!(s.controller.phase(), StreamPhase::Running);
    // Acknowledged only once every member showed a first image.
    assert!(s.fakes.observer.results(op).is_empty());
    s.members.emit(1, Some(SECOND), SECOND);
    s.members.emit(2, Some(SECOND), SECOND);
    s.pump();
    assert!(s.fakes.observer.results(op).is_empty());
    assert_eq!(
        s.compositor.composes(),
        0,
        "never composed with a member missing"
    );
    s.members.emit(3, Some(SECOND), SECOND);
    s.pump();
    assert_eq!(s.fakes.observer.results(op), vec![OpOutcome::Completed]);
    assert!(
        s.fakes
            .observer
            .jobs()
            .iter()
            .any(|j| matches!(j, ObserverJob::Started { .. }))
    );
    assert_eq!(s.compositor.composes(), 1);
    assert_eq!(lock(&s.compositor.composes)[0].len(), 3);
    assert_eq!(s.encoder().pending_seqs(), vec![1]);
    s.close();
    s.assert_clean();
}

#[test]
fn one_failed_member_start_fails_the_group_and_stops_only_the_started() {
    let s = Canvas::new();
    let op = s.controller.start();
    s.pump();
    s.members.answer(Op::Start, 1, None);
    s.members
        .answer(Op::Start, 2, Some(MediaError::PermissionDenied));
    s.members.answer(Op::Start, 3, None);
    s.pump();
    assert_eq!(
        s.fakes.observer.results(op),
        vec![OpOutcome::Rejected {
            code: "stream.start".into()
        }]
    );
    let stopped: HashSet<_> = s.members.ops(Op::Stop).iter().map(|(c, _)| *c).collect();
    assert_eq!(stopped, HashSet::from([capture_of(1), capture_of(3)]));
    assert!(
        !s.fakes
            .observer
            .jobs()
            .iter()
            .any(|j| matches!(j, ObserverJob::Started { .. }))
    );
    s.members.answer(Op::Stop, 1, None);
    s.pump();
    assert_eq!(s.controller.phase(), StreamPhase::Stopping);
    s.members.answer(Op::Stop, 3, None);
    s.pump();
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.close();
    s.assert_clean();
}

#[test]
fn a_member_that_cannot_open_rejects_the_start_with_nothing_started() {
    let s = Canvas::new();
    *lock(&s.members.fail_open) = Some(2);
    let op = s.controller.start();
    s.pump();
    assert_eq!(
        s.fakes.observer.results(op),
        vec![OpOutcome::Rejected {
            code: "stream.capture".into()
        }]
    );
    assert!(s.members.ops(Op::Start).is_empty());
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.close();
    s.assert_clean();
}

#[test]
fn the_group_stops_as_one_and_a_failed_member_stop_keeps_that_member() {
    let s = Canvas::new();
    s.running();
    let stop = s.controller.stop();
    s.pump();
    assert_eq!(s.members.ops(Op::Stop).len(), 3);
    assert_eq!(
        s.members.leases(),
        0,
        "member leases cleared before the stop"
    );
    s.members.answer(Op::Stop, 1, None);
    s.members.answer(Op::Stop, 2, Some(MediaError::Internal));
    s.pump();
    assert!(
        s.fakes.observer.results(stop).is_empty(),
        "waits for every member"
    );
    s.members.answer(Op::Stop, 3, None);
    s.pump();
    assert_eq!(
        s.fakes.observer.results(stop),
        vec![OpOutcome::Rejected {
            code: "stream.stop".into()
        }]
    );
    assert_eq!(s.controller.phase(), StreamPhase::StopFailed);
    // A retry stops only the member still running.
    let again = s.controller.stop();
    s.pump();
    let stops = s.members.ops(Op::Stop);
    assert_eq!(stops.len(), 4);
    assert_eq!(stops[3].0, capture_of(2));
    s.members.answer(Op::Stop, 2, None);
    s.pump();
    assert_eq!(s.fakes.observer.results(again), vec![OpOutcome::Completed]);
    assert_eq!(s.controller.phase(), StreamPhase::Idle);
    s.close();
    s.assert_clean();
}

#[test]
fn a_group_start_without_every_answer_is_unknown_after_10_s() {
    let s = Canvas::new();
    let op = s.controller.start();
    s.pump();
    s.members.answer(Op::Start, 1, None);
    s.members.answer(Op::Start, 2, None);
    s.advance_ms(10_001);
    assert_eq!(
        s.fakes.observer.results(op),
        vec![OpOutcome::OutcomeUnknown]
    );
    assert_eq!(s.controller.phase(), StreamPhase::StartUnknown);
    // The late answer settles it; the group is then stopped.
    s.members.answer(Op::Start, 3, None);
    s.pump();
    assert_eq!(s.members.ops(Op::Stop).len(), 3);
    s.close();
    s.assert_clean();
}

#[test]
fn rate_updates_reach_every_member_and_a_size_change_is_refused() {
    let s = Canvas::new();
    s.running();
    let resize = s.controller.update(StreamUpdate {
        rate: None,
        capture: CaptureUpdate {
            min_interval_ns: None,
            region: None,
            cursor: None,
        },
        encode_width: Some(800),
        encode_height: Some(600),
    });
    s.pump();
    assert_eq!(
        s.fakes.observer.results(resize),
        vec![OpOutcome::Rejected {
            code: "stream.config".into()
        }]
    );
    let rate = s.controller.update(StreamUpdate {
        rate: Some(RateUpdate {
            bitrate_bps: 1_000_000,
            fps: 30,
        }),
        capture: CaptureUpdate {
            min_interval_ns: Some(SECOND / 30),
            region: None,
            cursor: None,
        },
        encode_width: None,
        encode_height: None,
    });
    s.pump();
    assert_eq!(s.members.ops(Op::Update).len(), 3);
    for display in [1, 2, 3] {
        s.members.answer(Op::Update, display, None);
    }
    s.pump();
    assert_eq!(s.fakes.observer.results(rate), vec![OpOutcome::Completed]);
    s.close();
    s.assert_clean();
}

// ---------------------------------------------------------------------------
// First images and re-observation.

#[test]
fn a_member_without_a_first_image_fails_after_5_s_never_shown_black() {
    let s = Canvas::new();
    let op = s.start_group();
    s.members.emit(1, Some(SECOND), SECOND);
    s.members.emit(2, Some(SECOND), SECOND);
    s.pump();
    s.advance_ms(4_900);
    assert!(s.fakes.observer.failures().is_empty());
    assert_eq!(s.compositor.composes(), 0);
    s.advance_ms(200);
    assert_eq!(
        s.fakes.observer.failures(),
        vec!["stream.firstFrame".to_string()]
    );
    assert_eq!(
        s.fakes.observer.results(op),
        vec![OpOutcome::Rejected {
            code: "stream.firstFrame".into()
        }]
    );
    assert_eq!(s.compositor.composes(), 0);
    assert_eq!(s.members.ops(Op::Stop).len(), 3);
    s.close();
    s.assert_clean();
}

#[test]
fn the_layout_is_re_observed_every_250_ms() {
    let s = Canvas::new();
    s.running();
    s.advance_ms(1_000);
    assert!(s.fakes.observer.failures().is_empty());
    // Display 2 moves.
    lock(&s.members.observed).insert(
        2,
        Some(DisplayLayout {
            display_id: 2,
            pixel_width: 800,
            pixel_height: 600,
            scale: 1.0,
            origin_x: 900,
            origin_y: 0,
        }),
    );
    s.advance_ms(240);
    s.advance_ms(20);
    assert_eq!(
        s.fakes.observer.failures(),
        vec!["stream.layoutChanged".to_string()]
    );
    s.close();
    s.assert_clean();
}

#[test]
fn a_changed_layout_never_reaches_capture() {
    let s = Canvas::new();
    lock(&s.members.observed).insert(3, None);
    let op = s.controller.start();
    s.pump();
    assert_eq!(
        s.fakes.observer.results(op),
        vec![OpOutcome::Rejected {
            code: "stream.layoutChanged".into()
        }]
    );
    assert!(lock(&s.members.opened).is_empty());
    assert!(lock(&s.compositor.plans).is_empty());
    s.close();
    s.assert_clean();
}

#[test]
fn a_blank_member_a_gone_member_and_a_member_layout_event_fail_the_canvas() {
    let s = Canvas::new();
    s.running();
    s.members
        .emit_status(2, Some(2 * SECOND), 2 * SECOND, FrameStatus::Blank);
    s.pump();
    assert_eq!(
        s.fakes.observer.failures(),
        vec!["stream.memberUnavailable".to_string()]
    );
    s.close();
    s.assert_clean();

    let s = Canvas::new();
    s.running();
    // Idle frames keep the member's last image.
    s.members
        .emit_status(2, Some(2 * SECOND), 2 * SECOND, FrameStatus::Idle);
    s.pump();
    assert!(s.fakes.observer.failures().is_empty());
    lock(&s.members.events)
        .clone()
        .unwrap()
        .source_gone(capture_of(3), SourceGone::Unavailable);
    s.pump();
    assert_eq!(
        s.fakes.observer.failures(),
        vec!["stream.memberUnavailable".to_string()]
    );
    s.close();
    s.assert_clean();

    let s = Canvas::new();
    s.running();
    lock(&s.members.events).clone().unwrap().layout_changed(
        capture_of(1),
        DisplayLayout {
            display_id: 1,
            pixel_width: 1600,
            pixel_height: 1200,
            scale: 2.0,
            origin_x: 0,
            origin_y: 0,
        },
    );
    s.pump();
    assert_eq!(
        s.fakes.observer.failures(),
        vec!["stream.layoutChanged".to_string()]
    );
    s.close();
    s.assert_clean();
}

// ---------------------------------------------------------------------------
// Cadence, pool and time.

#[test]
fn composites_follow_changes_at_most_once_per_frame_interval() {
    let s = Canvas::new();
    s.running();
    assert_eq!(s.compositor.composes(), 1);
    // A burst of member images within one interval: one more composite, when due.
    for i in 0..5 {
        s.members
            .emit(1 + i % 3, Some(SECOND + i as u64 * MS), SECOND);
        s.pump();
    }
    assert_eq!(s.compositor.composes(), 1);
    s.advance_ms(17);
    assert_eq!(s.compositor.composes(), 2);
    // A still screen composes nothing.
    s.advance_ms(500);
    assert_eq!(s.compositor.composes(), 2);
    let stats = s.controller.stats();
    assert_eq!(stats.canvas_composed, 2);
    assert_eq!(stats.canvas_member_frames, 8);
    s.close();
    s.assert_clean();
}

#[test]
fn pool_exhaustion_retries_when_a_composite_is_released_not_on_member_arrival() {
    let s = Canvas::new();
    s.running();
    let encoder = s.encoder();
    // Three composites in flight.
    for step in 1..=2 {
        s.advance_ms(20);
        s.members.emit(1, Some(SECOND + step * 20 * MS), SECOND);
        s.pump();
    }
    assert_eq!(encoder.pending_seqs(), vec![1, 2, 3]);
    // The next compose finds the pool exhausted.
    s.compositor.exhaust.store(1, Ordering::SeqCst);
    s.advance_ms(20);
    s.members.emit(2, Some(SECOND + 80 * MS), SECOND);
    s.pump();
    assert_eq!(s.compositor.composes(), 4);
    assert_eq!(s.controller.stats().canvas_pool_exhausted, 1);
    // Member images do not retry it, nor does time.
    for step in 0..3 {
        s.advance_ms(20);
        s.members
            .emit(3, Some(SECOND + (100 + step * 20) * MS), SECOND);
        s.pump();
    }
    assert_eq!(s.compositor.composes(), 4, "no retry on member arrival");
    // A composite released (the encoder's output of frame 1): retried at once.
    assert!(encoder.complete(1));
    s.pump();
    assert_eq!(s.compositor.composes(), 5);
    assert!(s.compositor.peak_live.load(Ordering::SeqCst) <= 5);
    s.close();
    s.assert_clean();
}

#[test]
fn an_exhausted_pool_with_nothing_of_rusts_to_release_retries_after_an_interval() {
    let s = Canvas::new();
    s.compositor.exhaust.store(1, Ordering::SeqCst);
    s.running();
    assert_eq!(s.compositor.composes(), 1);
    assert!(s.encoder().pending_seqs().is_empty());
    s.advance_ms(20);
    assert_eq!(s.compositor.composes(), 2);
    assert_eq!(s.encoder().pending_seqs(), vec![1]);
    s.close();
    s.assert_clean();
}

#[test]
fn the_composite_h0_is_the_newest_changed_member_and_the_spread_is_measured() {
    let s = Canvas::new();
    let op = s.start_group();
    s.members.emit(1, Some(1_000 * MS), 1_001 * MS);
    s.members.emit(2, Some(1_010 * MS), 1_011 * MS);
    s.members.emit(3, Some(1_005 * MS), 1_006 * MS);
    s.pump();
    assert_eq!(s.fakes.observer.results(op), vec![OpOutcome::Completed]);
    let encoder = s.encoder();
    let h0 = |index: usize| lock(&encoder.submits)[index].capture_ns;
    assert_eq!(h0(0), 1_010 * MS);
    assert_eq!(s.controller.stats().canvas_age_spread_us, 10_000);
    // Only member 1 changes: its H0, even though member 2 is older.
    s.advance_ms(20);
    s.members.emit(1, Some(1_500 * MS), 1_501 * MS);
    s.pump();
    assert_eq!(h0(1), 1_500 * MS);
    // Only member 3 changes, older than member 1's last image: still its H0.
    s.advance_ms(20);
    s.members.emit(3, Some(1_400 * MS), 1_401 * MS);
    s.pump();
    assert_eq!(h0(2), 1_400 * MS);
    let stats = s.controller.stats();
    assert_eq!(stats.canvas_age_spread_us, 490_000, "1 500 ms - 1 010 ms");
    assert_eq!(
        stats.canvas_age_spread_max_us, 495_000,
        "1 500 ms - 1 005 ms, before"
    );
    s.close();
    s.assert_clean();
}

#[test]
fn member_leases_stay_within_two_and_composites_within_the_pool() {
    let s = Canvas::new();
    s.running();
    // Images arrive faster than the executor drains: the mailbox keeps one
    // undelivered image per member.
    for round in 0..20u64 {
        for display in [1, 2, 3] {
            s.members
                .emit(display, Some(SECOND + round * MS), SECOND + round * MS);
            s.members.emit(
                display,
                Some(SECOND + round * MS + 1),
                SECOND + round * MS + 1,
            );
        }
        s.advance_ms(17);
        if round % 4 == 0 {
            s.encoder().complete_all();
        }
    }
    assert!(s.members.peak_member_leases.load(Ordering::SeqCst) <= 2);
    // Three in flight, the latest still waiting for a permit and the one being
    // composed: the pool of permits + 2 is never exhausted by Rust alone.
    assert!(s.compositor.peak_live.load(Ordering::SeqCst) <= 3 + 2);
    assert_eq!(s.controller.stats().canvas_pool_exhausted, 0);
    s.close();
    s.assert_clean();
}

#[test]
fn recovery_re_encodes_the_latest_composite_without_composing() {
    let s = Canvas::new();
    s.running();
    let encoder = s.encoder();
    encoder.complete_all();
    s.pump();
    let composes = s.compositor.composes();
    s.controller.request_keyframe();
    s.pump();
    let submits = lock(&encoder.submits).clone();
    assert_eq!(submits.len(), 2);
    assert!(submits[1].force_keyframe);
    assert_eq!(s.compositor.composes(), composes);
    s.close();
    s.assert_clean();
}

// ---------------------------------------------------------------------------
// frames.v4.

#[test]
fn layout_info_and_the_content_epoch_go_out_under_frames_v4() {
    let s = Canvas::with(layout3(), |parts| {
        parts.plan.extensions = vec![FrameExtension::Content];
    });
    s.fakes.out.frames_v4.store(true, Ordering::SeqCst);
    // The sink reads frames.v4 when it is created: rebuild with it on.
    drop(s);
    let fakes = Fakes::new();
    fakes.out.frames_v4.store(true, Ordering::SeqCst);
    let members = FakeMembers::new(fakes.log.clone());
    let compositor = FakeCompositor::new(fakes.log.clone());
    let clock = Arc::new(ManualClock(AtomicU64::new(SECOND)));
    let port = StreamPort::new();
    let queue = ManualQueue::new(port.clone(), clock.clone());
    let size = canvas_size(layout3(), 1600).unwrap();
    let mut parts = fakes.parts(port, queue.clone(), canvas_plan(size));
    parts.spec = canvas_spec(&layout3(), size);
    parts.capture = members.clone();
    parts.plan.extensions = vec![FrameExtension::Content];
    let controller = StreamController::canvas_with_clock(
        parts,
        CanvasParts {
            layout: layout3(),
            compositor: compositor.clone(),
        },
        clock.clone(),
    )
    .unwrap();
    let s = Canvas {
        fakes,
        members,
        compositor,
        clock,
        queue,
        controller,
        displays: vec![1, 2, 3],
    };
    s.running();
    s.encoder().complete_all();
    s.pump();
    let control = lock(&s.fakes.out.state).control.clone();
    let layout = control
        .iter()
        .find(|m| m[0] == 0x15)
        .expect("a layoutInfo message");
    let info = streamcore_media::video_control::LayoutInfo::decode(&layout[5..]).unwrap();
    assert_eq!(info.stream_id, 1);
    assert_eq!(info.epoch, 1);
    assert_eq!((info.frame_width, info.frame_height), (1600, 1200));
    assert_eq!((info.source.width, info.source.height), (1600, 1200));
    assert_eq!((info.content.width, info.content.height), (1600, 1200));
    let frame = s
        .fakes
        .out
        .packets()
        .into_iter()
        .find(|p| !p.meta.config)
        .expect("a frame");
    assert_eq!(frame.meta.extensions.layout_epoch, Some(1));
    s.close();
    s.assert_clean();
}

#[test]
fn closing_releases_every_member_image_and_composite() {
    let s = Canvas::new();
    s.running();
    s.advance_ms(20);
    s.members.emit(1, Some(2 * SECOND), 2 * SECOND);
    s.pump();
    assert!(s.compositor.live() > 0 && s.members.leases() > 0);
    s.close();
    s.assert_clean();
    // Nothing composes after Closed.
    let composes = s.compositor.composes();
    s.members.emit(2, Some(3 * SECOND), 3 * SECOND);
    s.pump();
    assert_eq!(s.compositor.composes(), composes);
    let _ = &s.clock;
    assert_eq!(s.members.leases(), 0);
}
