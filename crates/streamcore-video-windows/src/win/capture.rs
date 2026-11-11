//! `CaptureBackend` over Windows Graphics Capture (design doc 4.2): a monitor captured into BGRA
//! textures on the stream's D3D11 device (the encoder's adapter).
//!
//! `open` and `start` run on the executor; `start` creates the frame pool with `Create`, which
//! binds `FrameArrived` to the calling thread's `DispatcherQueue`, so every frame arrives on the
//! executor thread while it pumps messages, outside a drain, and `CaptureEvents::frame` drains
//! inline (admission, convert, submit with no hop). The handler takes the newest frame, releases
//! older ones it finds queued (counted as pool backlog), and leases the frame to Rust until
//! `release`: WGC cannot reuse its buffer meanwhile, so the pool is sized for the latest slot plus
//! the permits in flight plus one buffer for WGC to write.

use super::surfaces::{Entry, Surfaces};
use super::sys::com::now_ns;
use super::sys::d3d::{
    Gpu, OutputDesc, capture_item_for_monitor, enumerate_adapters, texture_from_surface,
    texture_size,
};
use crate::clock::hundred_ns_to_ns;
use crate::trace::Recorder;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use streamcore_video::{
    CaptureBackend, CaptureCaps, CaptureEvents, CaptureId, CaptureSpec, CaptureTarget,
    CaptureTargetKind, CaptureUpdate, ColorPrimaries, ColorTags, CursorMode, DisplayLayout,
    FrameStatus, GpuFence, MediaError, OpToken, PixelFormat, Rotation, SerialQueue, SourceCatalog,
    SourceGone, SurfaceId, SurfaceInfo, SurfaceKind, TransferFunction, YCbCrMatrix,
};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::System::DispatcherQueue;
use windows::core::IInspectable;

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
}

struct Capture {
    spec: CaptureSpec,
    display: u32,
    events: Arc<dyn CaptureEvents>,
    item: GraphicsCaptureItem,
    size: SizeInt32,
    running: Option<Running>,
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
    outputs: Vec<OutputDesc>,
    state: Mutex<State>,
    me: Weak<WgcCapture>,
    /// Notes for the report (border or cursor settings the OS refused).
    notes: Mutex<Vec<String>>,
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
        scale: 1.0,
        origin_x: o.desktop[0],
        origin_y: o.desktop[1],
    }
}

impl WgcCapture {
    pub fn new(
        gpu: Arc<Gpu>,
        surfaces: Arc<Surfaces>,
        recorder: Option<Arc<Recorder>>,
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
            outputs,
            state: Mutex::new(State::default()),
            me: me.clone(),
            notes: Mutex::new(Vec::new()),
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

    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Capture surfaces Rust still holds (0 after a clean stop).
    pub fn leased(&self) -> usize {
        self.surfaces.len()
    }

    fn start_capture(&self, id: CaptureId) -> Result<(), MediaError> {
        // `Create` needs a DispatcherQueue on this thread: the executor thread has one.
        if DispatcherQueue::GetForCurrentThread().is_err() {
            return Err(MediaError::Unsupported);
        }
        let (item, size, spec) = {
            let state = self.lock();
            let cap = state.captures.get(&id).ok_or(MediaError::InvalidSettings)?;
            if cap.running.is_some() {
                return Err(MediaError::InvalidSettings);
            }
            (cap.item.clone(), cap.size, cap.spec.clone())
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
                    me.on_frame_arrived(id, pool);
                }
                Ok(())
            });
        let frame_token = pool
            .FrameArrived(&handler)
            .map_err(|_| MediaError::Internal)?;
        let me = self.me.clone();
        let closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            if let Some(me) = me.upgrade() {
                me.on_item_closed(id);
            }
            Ok(())
        });
        let closed_token = item.Closed(&closed).ok();
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|_| MediaError::Internal)?;
        if let Err(e) = session.SetIsBorderRequired(false) {
            self.note(format!("IsBorderRequired=false refused: {e}"));
        }
        if let Err(e) = session.SetIsCursorCaptureEnabled(spec.cursor == CursorMode::Composited) {
            self.note(format!("IsCursorCaptureEnabled refused: {e}"));
        }
        if spec.min_interval_ns > 0
            && let Err(e) = session.SetMinUpdateInterval(TimeSpan {
                Duration: i64::try_from(spec.min_interval_ns / 100).unwrap_or(i64::MAX),
            })
        {
            self.note(format!("MinUpdateInterval refused: {e}"));
        }
        session.StartCapture().map_err(|_| MediaError::Internal)?;
        let mut state = self.lock();
        if let Some(cap) = state.captures.get_mut(&id) {
            cap.running = Some(Running {
                pool,
                session,
                frame_token,
                closed_token,
            });
        }
        Ok(())
    }

    /// `FrameArrived`, on the executor thread (outside a drain).
    fn on_frame_arrived(&self, id: CaptureId, pool: &Direct3D11CaptureFramePool) {
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
        let content = frame.ContentSize().ok();
        let Ok(texture) = frame.Surface().and_then(|s| texture_from_surface(&s)) else {
            let _ = frame.Close();
            return;
        };
        let (width, height) = texture_size(&texture);
        let (events, resized) = {
            let mut state = self.lock();
            let Some(cap) = state.captures.get_mut(&id) else {
                let _ = frame.Close();
                return;
            };
            let resized = content
                .filter(|c| c.Width != cap.size.Width || c.Height != cap.size.Height)
                .map(|c| {
                    cap.size = c;
                    (c, cap.display, i32::from(cap.spec.queue_depth))
                });
            (cap.events.clone(), resized)
        };
        let surface = self.surfaces.insert(Entry::Capture {
            texture,
            frame,
            read_until: 0,
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
        if let Some((size, display, buffers)) = resized {
            // The source changed size: new buffers at the new size, and tell the stream.
            if let Ok(device) = self.gpu.winrt_device() {
                let _ = pool.Recreate(
                    &device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    buffers,
                    size,
                );
            }
            if let Some(o) = self.outputs.get(display as usize) {
                events.layout_changed(id, layout_of(display, o, Some(size)));
            }
        }
    }

    fn on_item_closed(&self, id: CaptureId) {
        let events = self.lock().captures.get(&id).map(|c| c.events.clone());
        if let Some(events) = events {
            events.source_gone(id, SourceGone::Closed);
        }
    }

    fn stop_capture(&self, id: CaptureId) -> Result<(), MediaError> {
        let (running, item) = {
            let mut state = self.lock();
            let cap = state
                .captures
                .get_mut(&id)
                .ok_or(MediaError::InvalidSettings)?;
            (cap.running.take(), cap.item.clone())
        };
        if let Some(r) = running {
            let _ = r.pool.RemoveFrameArrived(r.frame_token);
            if let Some(t) = r.closed_token {
                let _ = item.RemoveClosed(t);
            }
            let _ = r.session.Close();
            let _ = r.pool.Close();
        }
        Ok(())
    }
}

impl CaptureBackend for WgcCapture {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            // A region captures its whole display; the converter crops `ConvertJob::source`,
            // which follows live moves.
            target_kinds: vec![CaptureTargetKind::Display, CaptureTargetKind::Region],
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
        let display_id = match spec.target {
            CaptureTarget::Display { display_id } | CaptureTarget::Region { display_id, .. } => {
                display_id
            }
            _ => return Err(MediaError::Unsupported),
        };
        let output = self
            .outputs
            .get(display_id as usize)
            .ok_or(MediaError::SourceGone)?;
        let item = capture_item_for_monitor(output.hmonitor).map_err(|_| MediaError::SourceGone)?;
        let size = item.Size().map_err(|_| MediaError::SourceGone)?;
        let mut state = self.lock();
        state.next += 1;
        let id = state.next;
        state.captures.insert(
            id,
            Capture {
                spec,
                display: display_id,
                events,
                item,
                size,
                running: None,
            },
        );
        Ok(id)
    }

    fn start(&self, id: CaptureId, op: OpToken) {
        let result = self.start_capture(id);
        let events = self.lock().captures.get(&id).map(|c| c.events.clone());
        if let Some(events) = events {
            events.op_done(op, result.err());
        }
    }

    fn update(&self, id: CaptureId, update: CaptureUpdate, op: OpToken) {
        let (events, session, region) = {
            let state = self.lock();
            let Some(cap) = state.captures.get(&id) else {
                return;
            };
            (
                cap.events.clone(),
                cap.running.as_ref().map(|r| r.session.clone()),
                matches!(cap.spec.target, CaptureTarget::Region { .. }),
            )
        };
        let mut error = None;
        // A region move needs nothing here: the capture holds the whole display and the
        // executor crops the new rect from the next convert on.
        if update.region.is_some() && !region {
            error = Some(MediaError::Unsupported);
        } else if let Some(session) = session {
            if let Some(ns) = update.min_interval_ns
                && session
                    .SetMinUpdateInterval(TimeSpan {
                        Duration: i64::try_from(ns / 100).unwrap_or(i64::MAX),
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
        let result = self.stop_capture(id);
        let events = self.lock().captures.get(&id).map(|c| c.events.clone());
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
