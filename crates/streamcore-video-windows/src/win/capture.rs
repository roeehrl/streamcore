//! `CaptureBackend` over Windows Graphics Capture (design doc 4.2): a monitor, or one window,
//! captured into BGRA textures on the stream's D3D11 device (the encoder's adapter).
//!
//! `open` and `start` run on the executor; `start` creates the frame pool with `Create`, which
//! binds `FrameArrived` to the calling thread's `DispatcherQueue`, so every frame arrives on the
//! executor thread while it pumps messages, outside a drain, and `CaptureEvents::frame` drains
//! inline (admission, convert, submit with no hop). The handler takes the newest frame, releases
//! older ones it finds queued (counted as pool backlog), and leases the frame to Rust until
//! `release`: WGC cannot reuse its buffer meanwhile, so the pool is sized for the latest slot plus
//! the permits in flight plus one buffer for WGC to write.
//!
//! **A window target** (`CaptureTarget::Window`) names its window by native id
//! ([`crate::window::handle_from_native_id`]) and runs the same path with a capture item made
//! for the window:
//!
//! - The picture is the window as DWM draws it (its frame and client area, the rectangle
//!   `DWMWA_EXTENDED_FRAME_BOUNDS` reports), upright, without the windows in front of it. Other
//!   top-level windows of the same app (menus, tooltips, dialogs) are not part of it.
//! - A size change arrives as frames whose content size differs from the pool's. The capture
//!   adopts the new size once it has held for `WINDOW_RESIZE_SETTLE_NS`: the pool is recreated
//!   and `CaptureEvents::layout_changed` reports the window's layout, once per resize however
//!   many sizes a drag went through. Until then frames are delivered from the old buffers (a
//!   window that grew shows the part that fits them). A `DispatcherQueueTimer` on the executor
//!   thread adopts the size when no further frame arrives, so a one-step resize (maximize, snap)
//!   waits the settle time and no longer. A display's mode change is adopted at once, as
//!   before. The decisions are `crate::window::SizeTracker`'s. A move alone produces no frame
//!   and no event: a host that maps input reads the window's bounds itself.
//! - The window closing raises the item's `Closed`, reported as `SourceGone::Closed`, once. A
//!   window that closed before `start` subscribed to `Closed` is found by asking again right
//!   after subscribing, and reported the same way after the start's result.
//! - Frames that arrive while the window is minimized are dropped, so the stream keeps its last
//!   picture, as on an idle screen, and frames resume when the window is restored.
//! - A window that is minimized when the capture opens may have no size: its pool then holds a
//!   1 x 1 placeholder, the first frame that carries a size only sizes the pool, and delivery
//!   starts with the next one.
//! - A region of a window is not offered: `CaptureTarget::Region` names a display.

use super::surfaces::{Entry, Surfaces};
use super::sys::com::{TimerPeriod, now_ns};
use super::sys::d3d;
use super::sys::d3d::{
    Gpu, OutputDesc, capture_item_for_monitor, enumerate_adapters, texture_from_surface,
    texture_size,
};
use super::sys::hwnd;
use crate::clock::hundred_ns_to_ns;
use crate::trace::Recorder;
use crate::window::{
    SizeTracker, TimerDecision, WINDOW_RESIZE_SETTLE_NS, handle_from_native_id, image_size,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use streamcore_video::{
    CaptureBackend, CaptureCaps, CaptureEvents, CaptureId, CaptureSpec, CaptureTarget,
    CaptureTargetKind, CaptureUpdate, ColorPrimaries, ColorTags, CursorMode, DisplayLayout,
    DisplayRotation, FrameStatus, GpuFence, MediaError, OpToken, PixelFormat, Rotation,
    SerialQueue, SourceCatalog, SourceGone, SurfaceId, SurfaceInfo, SurfaceKind, TransferFunction,
    YCbCrMatrix,
};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::System::{DispatcherQueue, DispatcherQueueTimer};
use windows::core::{HSTRING, IInspectable, h};

/// What WGC delivers for an SDR desktop: sRGB-encoded BGRA.
pub const SDR_DESKTOP: ColorTags = ColorTags {
    primaries: ColorPrimaries::Bt709,
    transfer: TransferFunction::Srgb,
    matrix: YCbCrMatrix::Identity,
    full_range: true,
};

struct Running {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    closed_token: Option<i64>,
    /// A window capture's settle timer and its `Tick` registration: one-shot, on the executor
    /// thread's `DispatcherQueue`, so it fires there like `FrameArrived`.
    settle: Option<(DispatcherQueueTimer, i64)>,
    /// The 1 ms timer period, held while this capture runs (dropped with `Running`).
    _timer: Option<TimerPeriod>,
}

/// What a capture shows.
#[derive(Clone, Copy)]
enum Source {
    /// A display, by its index in `WgcCapture::outputs`.
    Display(u32),
    /// A window, by its handle value.
    Window(isize),
}

struct Capture {
    spec: CaptureSpec,
    source: Source,
    events: Arc<dyn CaptureEvents>,
    item: GraphicsCaptureItem,
    /// The frame pool's size and when it adopts a new one.
    sizes: SizeTracker,
    /// `source_gone` was reported: it is reported once, whoever notices first.
    gone_reported: bool,
    running: Option<Running>,
}

/// What a capture did about its source's size, for a report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeCounts {
    /// Times the frame pool was given buffers of a new size (each reported as a layout).
    pub pool_resizes: u64,
    /// Frames delivered from the old buffers while a new size was settling.
    pub frames_while_settling: u64,
    /// Frames dropped because the window was minimized.
    pub frames_minimized: u64,
    /// Frames dropped before the pool had its first real size.
    pub frames_unsized: u64,
    /// How long after the last pool resize the next frame arrived, in ns; `None` when none has.
    pub frame_after_resize_ns: Option<u64>,
}

#[derive(Default)]
struct SizeLog {
    counts: SizeCounts,
    /// When the pool was last resized, until the next frame arrives.
    resized_at: Option<u64>,
}

#[derive(Default)]
struct State {
    next: CaptureId,
    captures: HashMap<CaptureId, Capture>,
}

pub struct WgcCapture {
    gpu: Arc<Gpu>,
    surfaces: Arc<Surfaces>,
    recorder: Option<Arc<Recorder>>,
    /// Hold the 1 ms timer period while a capture runs (`TimerPeriod`).
    timer_period: bool,
    outputs: Vec<OutputDesc>,
    state: Mutex<State>,
    me: Weak<WgcCapture>,
    /// Notes for the report (border or cursor settings the OS refused).
    notes: Mutex<Vec<String>>,
    sizes: Mutex<SizeLog>,
}

fn size_int(size: (i32, i32)) -> SizeInt32 {
    SizeInt32 {
        Width: size.0,
        Height: size.1,
    }
}

/// Starts (or restarts) a one-shot settle timer to fire after `after_ns`.
fn arm(timer: &DispatcherQueueTimer, after_ns: u64) {
    let _ = timer.SetInterval(TimeSpan {
        Duration: i64::try_from(after_ns / 100).unwrap_or(i64::MAX).max(1),
    });
    let _ = timer.Start();
}

fn layout_of(display: u32, o: &OutputDesc, size: Option<SizeInt32>) -> DisplayLayout {
    let (w, h) = size.map_or(
        (
            (o.desktop[2] - o.desktop[0]).max(0) as u32,
            (o.desktop[3] - o.desktop[1]).max(0) as u32,
        ),
        |s| (s.Width.max(0) as u32, s.Height.max(0) as u32),
    );
    DisplayLayout {
        display_id: display,
        pixel_width: w,
        pixel_height: h,
        scale: o.dpi.map_or(1.0, |dpi| f64::from(dpi) / 96.0),
        origin_x: o.desktop[0],
        origin_y: o.desktop[1],
        rotation: match o.rotation_deg {
            90 => DisplayRotation::Rotate90,
            180 => DisplayRotation::Rotate180,
            270 => DisplayRotation::Rotate270,
            _ => DisplayRotation::Identity,
        },
    }
}

/// A display's refresh interval in ns, read now from its active display path (the current
/// mode's exact rate: 59.94 Hz is 16 683 350 ns, not 1/59). For `StreamPlan::refresh_interval_ns`,
/// and for `StreamController::set_refresh_interval` after a mode change (`WM_DISPLAYCHANGE`).
/// `None` when the path or its rate cannot be read: the stream then uses its target interval.
pub fn refresh_interval_ns(output: &OutputDesc) -> Option<u64> {
    let ((num, den), _) = d3d::display_path(&output.device_name)?;
    crate::clock::interval_from_rational(num, den)
}

/// Whether this Windows has `GraphicsCaptureSession`'s property `name` (the capture border
/// switch came with Windows 11, the cursor switch with Windows 10 2004).
fn session_has(name: &HSTRING) -> bool {
    ApiInformation::IsPropertyPresent(h!("Windows.Graphics.Capture.GraphicsCaptureSession"), name)
        .unwrap_or(false)
}

impl WgcCapture {
    pub fn new(
        gpu: Arc<Gpu>,
        surfaces: Arc<Surfaces>,
        recorder: Option<Arc<Recorder>>,
        timer_period: bool,
    ) -> Result<Arc<Self>, String> {
        let outputs = enumerate_adapters()
            .map_err(|e| format!("enumerate adapters: {e}"))?
            .into_iter()
            .flat_map(|a| a.outputs)
            .collect();
        Ok(Arc::new_cyclic(|me| Self {
            gpu,
            surfaces,
            recorder,
            timer_period,
            outputs,
            state: Mutex::new(State::default()),
            me: me.clone(),
            notes: Mutex::new(Vec::new()),
            sizes: Mutex::new(SizeLog::default()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn note(&self, note: String) {
        self.notes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(note);
    }

    /// A note, unless the same one was already made (a failure retried every frame).
    fn note_once(&self, note: String) {
        let mut notes = self.notes.lock().unwrap_or_else(|p| p.into_inner());
        if !notes.contains(&note) {
            notes.push(note);
        }
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// What the captures did about their sources' sizes so far.
    pub fn size_counts(&self) -> SizeCounts {
        self.size_log().counts
    }

    fn size_log(&self) -> MutexGuard<'_, SizeLog> {
        self.sizes.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether `source` is a window that no longer exists.
    fn window_gone(source: Source) -> bool {
        matches!(source, Source::Window(handle) if !hwnd::exists(handle))
    }

    /// Capture surfaces Rust still holds (0 after a clean stop).
    pub fn leased(&self) -> usize {
        self.surfaces.len()
    }

    /// The output a source is shown on, with its index: the display itself, or the display
    /// most of the window is on. `None` for a display that is not listed, and for a window on a
    /// display that was attached after this backend was created.
    fn output_of(&self, source: Source) -> Option<(u32, &OutputDesc)> {
        match source {
            Source::Display(display) => self.outputs.get(display as usize).map(|o| (display, o)),
            Source::Window(handle) => {
                let monitor = hwnd::monitor(handle);
                self.outputs
                    .iter()
                    .enumerate()
                    .find(|(_, o)| o.hmonitor == monitor)
                    .map(|(i, o)| (i as u32, o))
            }
        }
    }

    /// The layout reported when a source changes size. A display's is its own. A window's is
    /// the window's: `size` pixels at the origin of its frame in desktop pixels, upright, with
    /// the scale and `display_id` of the display most of it is on (display 0 and scale 1 when
    /// that display is not listed).
    fn layout_for(&self, source: Source, size: SizeInt32) -> Option<DisplayLayout> {
        let output = self.output_of(source);
        match source {
            Source::Display(_) => output.map(|(display, o)| layout_of(display, o, Some(size))),
            Source::Window(handle) => {
                let origin = hwnd::frame_bounds(handle).map_or((0, 0), |b| (b[0], b[1]));
                Some(DisplayLayout {
                    display_id: output.map_or(0, |(display, _)| display),
                    pixel_width: size.Width.max(0) as u32,
                    pixel_height: size.Height.max(0) as u32,
                    scale: output
                        .and_then(|(_, o)| o.dpi)
                        .map_or(1.0, |dpi| f64::from(dpi) / 96.0),
                    origin_x: origin.0,
                    origin_y: origin.1,
                    rotation: DisplayRotation::Identity,
                })
            }
        }
    }

    /// The session's `MinUpdateInterval` (100 ns units) for a stream minimum of `min_ns` on
    /// `source`'s display: see [`crate::clock::capture_min_interval_ns`].
    fn session_min_interval(&self, source: Source, min_ns: u64) -> i64 {
        let refresh = self
            .output_of(source)
            .and_then(|(_, o)| refresh_interval_ns(o));
        let ns = crate::clock::capture_min_interval_ns(min_ns, refresh);
        i64::try_from(ns / 100).unwrap_or(i64::MAX)
    }

    fn start_capture(&self, id: CaptureId) -> Result<(), MediaError> {
        // `Create` needs a DispatcherQueue on this thread: the executor thread has one.
        if DispatcherQueue::GetForCurrentThread().is_err() {
            return Err(MediaError::Unsupported);
        }
        let (item, size, spec, source) = {
            let state = self.lock();
            let cap = state.captures.get(&id).ok_or(MediaError::InvalidSettings)?;
            if cap.running.is_some() {
                return Err(MediaError::InvalidSettings);
            }
            (
                cap.item.clone(),
                size_int(cap.sizes.pool()),
                cap.spec.clone(),
                cap.source,
            )
        };
        let device = self
            .gpu
            .winrt_device()
            .map_err(|_| MediaError::DeviceLost)?;
        let pool = Direct3D11CaptureFramePool::Create(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            i32::from(spec.queue_depth),
            size,
        )
        .map_err(|_| MediaError::OutOfResources)?;
        let me = self.me.clone();
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                if let (Some(me), Some(pool)) = (me.upgrade(), pool.as_ref()) {
                    me.on_frame_arrived(id, pool, source);
                }
                Ok(())
            });
        let frame_token = match pool.FrameArrived(&handler) {
            Ok(t) => t,
            Err(_) => {
                let _ = pool.Close();
                return Err(MediaError::Internal);
            }
        };
        let me = self.me.clone();
        let closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            if let Some(me) = me.upgrade() {
                me.on_item_closed(id);
            }
            Ok(())
        });
        let closed_token = item.Closed(&closed).ok();
        // A window's sizes settle on a timer of this thread's queue (`on_settle_timer`).
        let settle = match source {
            Source::Window(_) => self.settle_timer(id),
            Source::Display(_) => None,
        };
        // A start that fails past this point unregisters and closes what it made. A window that
        // closed before `Closed` was subscribed raised it to no one: what fails on its dead
        // item is a source that is gone, not an internal error.
        let failed = || {
            if Self::window_gone(source) {
                MediaError::SourceGone
            } else {
                MediaError::Internal
            }
        };
        let undo = |session: Option<&GraphicsCaptureSession>| {
            if let Some((timer, tick)) = &settle {
                let _ = timer.Stop();
                let _ = timer.RemoveTick(*tick);
            }
            let _ = pool.RemoveFrameArrived(frame_token);
            if let Some(t) = closed_token {
                let _ = item.RemoveClosed(t);
            }
            if let Some(s) = session {
                let _ = s.Close();
            }
            let _ = pool.Close();
        };
        let session = match pool.CreateCaptureSession(&item) {
            Ok(s) => s,
            Err(_) => {
                undo(None);
                return Err(failed());
            }
        };
        // The yellow capture border and the cursor are switched only where this Windows has
        // the switch; without it the border stays and the cursor is drawn, and a note says so.
        if !session_has(h!("IsBorderRequired")) {
            self.note("IsBorderRequired is not on this Windows: the capture border stays".into());
        } else if let Err(e) = session.SetIsBorderRequired(false) {
            self.note(format!("IsBorderRequired=false refused: {e}"));
        }
        if !session_has(h!("IsCursorCaptureEnabled")) {
            self.note("IsCursorCaptureEnabled is not on this Windows: the cursor is drawn".into());
        } else if let Err(e) =
            session.SetIsCursorCaptureEnabled(spec.cursor == CursorMode::Composited)
        {
            self.note(format!("IsCursorCaptureEnabled refused: {e}"));
        }
        if spec.min_interval_ns > 0
            && let Err(e) = session.SetMinUpdateInterval(TimeSpan {
                Duration: self.session_min_interval(source, spec.min_interval_ns),
            })
        {
            self.note(format!("MinUpdateInterval refused: {e}"));
        }
        if session.StartCapture().is_err() {
            undo(Some(&session));
            return Err(failed());
        }
        let timer = self.timer_period.then(TimerPeriod::acquire);
        let running = Running {
            pool,
            session,
            frame_token,
            closed_token,
            settle,
            _timer: timer,
        };
        let mut state = self.lock();
        match state.captures.get_mut(&id) {
            Some(cap) => cap.running = Some(running),
            None => {
                drop(state);
                stop_running(running, &item);
            }
        }
        Ok(())
    }

    /// A one-shot timer on the calling (executor) thread's `DispatcherQueue` whose tick runs
    /// `on_settle_timer`. Without one (it could not be made) a new size is adopted by the first
    /// frame that arrives after the settle time, and a note says so.
    fn settle_timer(&self, id: CaptureId) -> Option<(DispatcherQueueTimer, i64)> {
        let me = self.me.clone();
        let tick = TypedEventHandler::<DispatcherQueueTimer, IInspectable>::new(move |_, _| {
            if let Some(me) = me.upgrade() {
                me.on_settle_timer(id);
            }
            Ok(())
        });
        let made = DispatcherQueue::GetForCurrentThread()
            .and_then(|queue| queue.CreateTimer())
            .and_then(|timer| {
                timer.SetIsRepeating(false)?;
                let token = timer.Tick(&tick)?;
                Ok((timer, token))
            });
        match made {
            Ok(timer) => Some(timer),
            Err(e) => {
                self.note(format!(
                    "no settle timer ({e}): a new window size waits for a frame"
                ));
                None
            }
        }
    }

    /// `FrameArrived`, on the executor thread (outside a drain).
    fn on_frame_arrived(&self, id: CaptureId, pool: &Direct3D11CaptureFramePool, source: Source) {
        let h1 = now_ns();
        let mut newest = None;
        let mut backlog = 0u64;
        while let Ok(frame) = pool.TryGetNextFrame() {
            if let Some(older) = newest.replace(frame) {
                let _ = older.Close();
                backlog += 1;
            }
        }
        let Some(frame) = newest else { return };
        let h0 = frame
            .SystemRelativeTime()
            .ok()
            .map(|t| hundred_ns_to_ns(t.Duration));
        let content = frame.ContentSize().ok().map(|c| (c.Width, c.Height));
        // Asked with no lock held; a display is never minimized.
        let minimized = matches!(source, Source::Window(handle) if hwnd::is_minimized(handle));
        let (events, buffers, decision, settling, unsized_pool, timer) = {
            let mut state = self.lock();
            let Some(cap) = state.captures.get_mut(&id) else {
                let _ = frame.Close();
                return;
            };
            let before = cap.sizes.pool();
            let decision = cap.sizes.on_frame(content, minimized, h1);
            let timer = decision
                .arm_ns
                .and(cap.running.as_ref())
                .and_then(|r| r.settle.as_ref())
                .map(|(timer, _)| timer.clone());
            (
                cap.events.clone(),
                i32::from(cap.spec.queue_depth),
                decision,
                // Delivered from buffers of a size the source no longer has.
                decision.deliver && content.is_some_and(|c| c.0 > 0 && c.1 > 0 && c != before),
                !decision.deliver && !minimized,
                timer,
            )
        };
        {
            let mut log = self.size_log();
            if let Some(at) = log.resized_at.take() {
                log.counts.frame_after_resize_ns = Some(h1.saturating_sub(at));
            }
            log.counts.frames_while_settling += u64::from(settling && decision.resize.is_none());
            log.counts.frames_minimized += u64::from(minimized);
            log.counts.frames_unsized += u64::from(unsized_pool);
        }
        if let (Some(timer), Some(after_ns)) = (&timer, decision.arm_ns) {
            arm(timer, after_ns);
        }
        if !decision.deliver {
            // Minimized, or the pool still has its placeholder size: nothing to show. A frame
            // that gives the first real size sizes the pool, and the next one is delivered.
            let _ = frame.Close();
            if let Some(size) = decision.resize {
                self.resize_pool(id, pool, size, buffers, source, &events);
            }
            return;
        }
        let Ok(texture) = frame.Surface().and_then(|s| texture_from_surface(&s)) else {
            let _ = frame.Close();
            return;
        };
        // The image's size: a frame of a new size still comes in a buffer of the pool's old
        // size, with the image in its top-left part (clipped when it grew).
        let (width, height) = image_size(content, texture_size(&texture));
        let surface = self.surfaces.insert(Entry::Capture {
            texture,
            frame,
            read_until: 0,
            content: (width, height),
        });
        if let Some(r) = &self.recorder {
            r.captured(surface, h0, h1, backlog);
        }
        let info = SurfaceInfo {
            id: surface,
            kind: SurfaceKind::D3d11Texture {
                adapter_luid: self.gpu.luid,
            },
            format: PixelFormat::Bgra,
            width,
            height,
            color: SDR_DESKTOP,
            capture_ns: h1,
            source_present_ns: h0,
            dirty: None,
            rotation: Rotation::None,
            fence: GpuFence::None,
        };
        if backlog > 0 {
            // A leaf call: counts into `StatsSnapshot::capture_skipped`.
            events.skipped(id, None, u32::try_from(backlog).unwrap_or(u32::MAX));
        }
        // No lock held: this may drain the executor inline (convert and submit run now).
        events.frame(id, None, info, FrameStatus::Complete);
        if let Some(size) = decision.resize {
            self.resize_pool(id, pool, size, buffers, source, &events);
        }
    }

    /// A window capture's settle timer fired, on the executor thread (outside a drain): the
    /// size the last frames showed has held, or changed again and needs more time.
    fn on_settle_timer(&self, id: CaptureId) {
        let (decision, pool, timer, events, source, buffers) = {
            let mut state = self.lock();
            let Some(cap) = state.captures.get_mut(&id) else {
                return;
            };
            let Some(running) = cap.running.as_ref() else {
                return;
            };
            let pool = running.pool.clone();
            let timer = running.settle.as_ref().map(|(timer, _)| timer.clone());
            (
                cap.sizes.on_timer(now_ns()),
                pool,
                timer,
                cap.events.clone(),
                cap.source,
                i32::from(cap.spec.queue_depth),
            )
        };
        match decision {
            TimerDecision::Idle => {}
            TimerDecision::Wait(after_ns) => {
                if let Some(timer) = &timer {
                    arm(timer, after_ns);
                }
            }
            TimerDecision::Resize(size) => {
                self.resize_pool(id, &pool, size, buffers, source, &events)
            }
        }
    }

    /// The source has a new size: new buffers at that size, and tell the stream. The tracker
    /// moves only once the pool has them, so a failed `Recreate` is decided again by the next
    /// frame. On the executor thread, where the pool was created.
    fn resize_pool(
        &self,
        id: CaptureId,
        pool: &Direct3D11CaptureFramePool,
        size: (i32, i32),
        buffers: i32,
        source: Source,
        events: &Arc<dyn CaptureEvents>,
    ) {
        let recreated = self.gpu.winrt_device().and_then(|device| {
            pool.Recreate(
                &device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                buffers,
                size_int(size),
            )
        });
        if let Err(e) = recreated {
            self.note_once(format!(
                "frame pool Recreate at {}x{} failed: {e}",
                size.0, size.1
            ));
            return;
        }
        if let Some(cap) = self.lock().captures.get_mut(&id) {
            cap.sizes.resized(size);
        }
        {
            let mut log = self.size_log();
            log.counts.pool_resizes += 1;
            log.resized_at = Some(now_ns());
        }
        if let Some(layout) = self.layout_for(source, size_int(size)) {
            events.layout_changed(id, layout);
        }
    }

    /// The source is gone (the item's `Closed`, or a window found closed at start): reported
    /// once per capture.
    fn on_item_closed(&self, id: CaptureId) {
        let events = {
            let mut state = self.lock();
            match state.captures.get_mut(&id) {
                Some(cap) if !cap.gone_reported => {
                    cap.gone_reported = true;
                    Some(cap.events.clone())
                }
                _ => None,
            }
        };
        if let Some(events) = events {
            events.source_gone(id, SourceGone::Closed);
        }
    }

    /// Stops and forgets the capture: a stopped capture is not started again (the stream opens
    /// a new one).
    fn stop_capture(&self, id: CaptureId) -> Result<(), MediaError> {
        let cap = self
            .lock()
            .captures
            .remove(&id)
            .ok_or(MediaError::InvalidSettings)?;
        if let Some(r) = cap.running {
            stop_running(r, &cap.item);
        }
        Ok(())
    }
}

/// Unregisters a running capture's handlers and closes its session and pool (its timer hold
/// ends with it).
fn stop_running(r: Running, item: &GraphicsCaptureItem) {
    if let Some((timer, tick)) = &r.settle {
        let _ = timer.Stop();
        let _ = timer.RemoveTick(*tick);
    }
    let _ = r.pool.RemoveFrameArrived(r.frame_token);
    if let Some(t) = r.closed_token {
        let _ = item.RemoveClosed(t);
    }
    let _ = r.session.Close();
    let _ = r.pool.Close();
}

impl CaptureBackend for WgcCapture {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            // A region captures its whole display; the converter crops `ConvertJob::source`,
            // which follows live moves. A window is captured whole.
            target_kinds: vec![
                CaptureTargetKind::Display,
                CaptureTargetKind::Window,
                CaptureTargetKind::Region,
            ],
            // The backend never moves, resizes or raises the window it captures.
            window_fit: false,
            live_region_move: true,
            cursor_modes: vec![CursorMode::Hidden, CursorMode::Composited],
            surface_kinds: vec![SurfaceKind::D3d11Texture {
                adapter_luid: self.gpu.luid,
            }],
            pool_depth: 8,
            virtual_displays: false,
            region_in_surface: false,
        }
    }

    /// The displays. No windows are listed, although a window can be captured: which windows
    /// to offer (the ones a switcher shows, with their apps) is a rule about the desktop, and
    /// a host takes that list from where it reads the desktop (`streamcore-input-windows`'
    /// `desktop::running_apps`, whose `WindowId::native_id` is the `window_id` to open).
    fn enumerate(&self) -> Result<SourceCatalog, MediaError> {
        Ok(SourceCatalog {
            displays: self
                .outputs
                .iter()
                .enumerate()
                .map(|(i, o)| layout_of(i as u32, o, None))
                .collect(),
            windows: Vec::new(),
        })
    }

    fn observe_layout(&self, display: u32) -> Option<DisplayLayout> {
        self.outputs
            .get(display as usize)
            .map(|o| layout_of(display, o, None))
    }

    fn open(
        &self,
        spec: CaptureSpec,
        _queue: Arc<dyn SerialQueue>,
        events: Arc<dyn CaptureEvents>,
    ) -> Result<CaptureId, MediaError> {
        // Frames are delivered on the thread that starts the capture, which must be the queue's
        // own thread with a DispatcherQueue (`ExecutorThread`); `open` runs there too.
        if DispatcherQueue::GetForCurrentThread().is_err() {
            return Err(MediaError::Unsupported);
        }
        if spec.adapter_luid.is_some_and(|l| l != self.gpu.luid) {
            return Err(MediaError::Unsupported);
        }
        let (source, item) = match spec.target {
            CaptureTarget::Display { display_id } | CaptureTarget::Region { display_id, .. } => {
                let output = self
                    .outputs
                    .get(display_id as usize)
                    .ok_or(MediaError::SourceGone)?;
                let item = capture_item_for_monitor(output.hmonitor)
                    .map_err(|_| MediaError::SourceGone)?;
                (Source::Display(display_id), item)
            }
            CaptureTarget::Window { window_id } => {
                // An id that is no handle here is a wrong setting, and so is a window that is
                // there but that WGC will not capture; a window that has closed is a source
                // that is gone.
                let handle = handle_from_native_id(window_id).ok_or(MediaError::InvalidSettings)?;
                if !hwnd::exists(handle) {
                    return Err(MediaError::SourceGone);
                }
                let item = hwnd::capture_item(handle).map_err(|_| {
                    if hwnd::exists(handle) {
                        MediaError::InvalidSettings
                    } else {
                        MediaError::SourceGone
                    }
                })?;
                (Source::Window(handle), item)
            }
            _ => return Err(MediaError::Unsupported),
        };
        let size = item.Size().map_err(|_| MediaError::SourceGone)?;
        // A window's sizes settle before the pool follows; a display's mode change is one
        // step and is adopted at once. An item with no size yet gets a placeholder pool.
        let sizes = SizeTracker::new(
            (size.Width, size.Height),
            match source {
                Source::Window(_) => WINDOW_RESIZE_SETTLE_NS,
                Source::Display(_) => 0,
            },
        );
        let mut state = self.lock();
        state.next += 1;
        let id = state.next;
        state.captures.insert(
            id,
            Capture {
                spec,
                source,
                events,
                item,
                sizes,
                gone_reported: false,
                running: None,
            },
        );
        Ok(id)
    }

    fn start(&self, id: CaptureId, op: OpToken) {
        let result = self.start_capture(id);
        let started = result.is_ok();
        let capture = self
            .lock()
            .captures
            .get(&id)
            .map(|c| (c.events.clone(), c.source));
        let Some((events, source)) = capture else {
            return;
        };
        events.op_done(op, result.err());
        // `Closed` is subscribed now. A window that closed before that raised it to no one,
        // so ask once more: it is reported as any closed source is, after the start's result.
        if started && Self::window_gone(source) {
            self.on_item_closed(id);
        }
    }

    fn update(&self, id: CaptureId, update: CaptureUpdate, op: OpToken) {
        let (events, session, region, source) = {
            let state = self.lock();
            let Some(cap) = state.captures.get(&id) else {
                return;
            };
            (
                cap.events.clone(),
                cap.running.as_ref().map(|r| r.session.clone()),
                matches!(cap.spec.target, CaptureTarget::Region { .. }),
                cap.source,
            )
        };
        let mut error = None;
        // A region move needs nothing here: the capture holds the whole display and the
        // executor crops the new rect from the next convert on. A window has no region.
        if update.region.is_some() && !region {
            error = Some(MediaError::Unsupported);
        } else if let Some(session) = session {
            if let Some(ns) = update.min_interval_ns
                && session
                    .SetMinUpdateInterval(TimeSpan {
                        Duration: self.session_min_interval(source, ns),
                    })
                    .is_err()
            {
                error = Some(MediaError::Unsupported);
            }
            if let Some(cursor) = update.cursor
                && session
                    .SetIsCursorCaptureEnabled(cursor == CursorMode::Composited)
                    .is_err()
            {
                error = Some(MediaError::Unsupported);
            }
        }
        events.op_done(op, error);
    }

    fn stop(&self, id: CaptureId, op: OpToken) {
        let events = self.lock().captures.get(&id).map(|c| c.events.clone());
        let result = self.stop_capture(id);
        if let Some(events) = events {
            events.op_done(op, result.err());
        }
    }

    fn release(&self, surface: SurfaceId) {
        // A leaf: the table locks only. The WGC frame closes after they are released, or, while
        // a recorded convert still reads it, when the converter's fence passes (`Surfaces::retire`).
        let Some(entry) = self.surfaces.remove(surface) else {
            return;
        };
        if let Some(r) = &self.recorder {
            r.capture_released(surface);
        }
        drop(self.surfaces.retire(entry));
    }
}
