//! `Converter` over `ID3D11VideoProcessor` (design doc 4.2): BGRA -> NV12 at the target size in
//! one pass, with the output colour space from the job's tags, into a pool of NV12 textures.
//!
//! `convert` records the blit, signals the device fence with a new value and flushes, and returns
//! at once with `GpuFence::D3d11Fence { shared_handle: 0, value }`: the fence is this device's,
//! in process, so no shared handle is needed (0 means "the stream device's fence"). The encoder
//! makes the GPU wait for that value before it reads the texture. A fence thread waits for each
//! value with `SetEventOnCompletion` and stamps H2, and in synthetic mode reads the barcode back
//! from a two-row copy recorded before the signal, so the executor never waits on the GPU.

use super::surfaces::{Entry, Surfaces};
use super::sys::com::{Event, Wake, now_ns, wait};
use super::sys::d3d::{Fence, Gpu, Readback, texture_size};
use super::sys::vp::{DestRect, VideoProcessor, letterbox};
use crate::barcode;
use crate::trace::Recorder;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use streamcore_video::{
    ConvertJob, Converter, GpuFence, MediaError, PixelFormat, Rotation, SurfaceId, SurfaceInfo,
    SurfaceKind, YCbCrMatrix,
};
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, ID3D11Texture2D};

/// Where the synthetic barcode lands in the converted frame.
#[derive(Clone, Copy, Debug)]
pub struct Strip {
    /// Even luma row through the middle of the code.
    pub y: u32,
    pub x0: f64,
    pub width: f64,
}

struct Slot {
    texture: ID3D11Texture2D,
    view: usize,
    busy: bool,
}

struct Pipeline {
    input: (u32, u32),
    output: (u32, u32),
    vp: VideoProcessor,
    slots: Vec<Slot>,
}

struct Watch {
    value: u64,
    readback: Option<(Readback, Strip)>,
}

struct FenceThread {
    tx: Option<mpsc::Sender<Watch>>,
    join: Option<JoinHandle<()>>,
}

pub struct VpConverter {
    gpu: Arc<Gpu>,
    surfaces: Arc<Surfaces>,
    recorder: Option<Arc<Recorder>>,
    fence: Arc<Fence>,
    next_value: AtomicU64,
    pool_size: usize,
    fps: u32,
    pipeline: Mutex<Option<Pipeline>>,
    strip: Mutex<Option<Strip>>,
    readbacks: Arc<Mutex<Vec<Readback>>>,
    thread: Mutex<FenceThread>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl VpConverter {
    /// `pool_size` NV12 outputs: at least the permits in flight plus one being converted.
    pub fn new(
        gpu: Arc<Gpu>,
        surfaces: Arc<Surfaces>,
        recorder: Option<Arc<Recorder>>,
        pool_size: usize,
        fps: u32,
    ) -> Result<Arc<Self>, String> {
        let fence = Arc::new(Fence::new(&gpu).map_err(|e| format!("ID3D11Fence: {e}"))?);
        let readbacks: Arc<Mutex<Vec<Readback>>> = Arc::default();
        let (tx, rx) = mpsc::channel::<Watch>();
        let event = Event::new().map_err(|e| format!("event: {e}"))?;
        let (f, g, r, pool) = (
            fence.clone(),
            gpu.clone(),
            recorder.clone(),
            readbacks.clone(),
        );
        let join = std::thread::Builder::new()
            .name("convert-fence".into())
            .spawn(move || fence_main(&rx, &f, &g, r.as_deref(), &pool, &event))
            .map_err(|e| format!("spawn fence thread: {e}"))?;
        Ok(Arc::new(Self {
            gpu,
            surfaces,
            recorder,
            fence,
            next_value: AtomicU64::new(1),
            pool_size: pool_size.max(2),
            fps,
            pipeline: Mutex::new(None),
            strip: Mutex::new(None),
            readbacks,
            thread: Mutex::new(FenceThread {
                tx: Some(tx),
                join: Some(join),
            }),
        }))
    }

    /// The device fence the converter signals (the encoder waits on it).
    pub fn fence(&self) -> Arc<Fence> {
        self.fence.clone()
    }

    /// Synthetic mode: read the barcode back from every converted frame.
    pub fn set_readback(&self, strip: Strip, width: u32, copies: usize) -> Result<(), String> {
        let mut pool = lock(&self.readbacks);
        for _ in 0..copies {
            pool.push(Readback::new(&self.gpu, width).map_err(|e| format!("readback: {e}"))?);
        }
        *lock(&self.strip) = Some(strip);
        Ok(())
    }

    /// Where a `source`-sized input lands in a `target`-sized output.
    pub fn dest(source: (u32, u32), target: (u32, u32)) -> DestRect {
        letterbox(source, target)
    }

    /// Whether frames had to be copied before conversion (the input could not be bound).
    pub fn copied_inputs(&self) -> u64 {
        lock(&self.pipeline).as_ref().map_or(0, |p| p.vp.copies)
    }

    /// Stops the fence thread after the stream has closed.
    pub fn shutdown(&self) {
        let (tx, join) = {
            let mut t = lock(&self.thread);
            (t.tx.take(), t.join.take())
        };
        drop(tx);
        if let Some(j) = join {
            let _ = j.join();
        }
    }

    fn pipeline_for<'a>(
        &self,
        slot: &'a mut Option<Pipeline>,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<&'a mut Pipeline, MediaError> {
        let fits = slot
            .as_ref()
            .is_some_and(|p| p.input == input && p.output == output);
        if !fits {
            if slot
                .as_ref()
                .is_some_and(|p| p.slots.iter().any(|s| s.busy))
            {
                // Old outputs are still leased; wait for their release.
                return Err(MediaError::OutOfResources);
            }
            let mut vp = VideoProcessor::new(&self.gpu, input, output, self.fps)
                .map_err(|_| MediaError::Unsupported)?;
            let mut slots = Vec::new();
            for _ in 0..self.pool_size {
                let texture = self
                    .gpu
                    .create_nv12(output.0, output.1, D3D11_BIND_RENDER_TARGET, None)
                    .map_err(|_| MediaError::OutOfResources)?;
                let view = vp
                    .add_output(&texture)
                    .map_err(|_| MediaError::OutOfResources)?;
                slots.push(Slot {
                    texture,
                    view,
                    busy: false,
                });
            }
            *slot = Some(Pipeline {
                input,
                output,
                vp,
                slots,
            });
        }
        slot.as_mut().ok_or(MediaError::Internal)
    }
}

fn fence_main(
    rx: &mpsc::Receiver<Watch>,
    fence: &Fence,
    gpu: &Gpu,
    recorder: Option<&Recorder>,
    pool: &Mutex<Vec<Readback>>,
    event: &Event,
) {
    while let Ok(w) = rx.recv() {
        if fence.completed() < w.value {
            if fence.notify(w.value, event).is_err() {
                continue;
            }
            // Bounded: a lost device must not wedge the thread.
            while fence.completed() < w.value {
                if wait(&[event.raw()], 1000) == Wake::Timeout && fence.completed() < w.value {
                    break;
                }
            }
        }
        let at = now_ns();
        if let Some(r) = recorder {
            r.fence_done(w.value, at);
        }
        if let Some((rb, strip)) = w.readback {
            // The copy was recorded before the signal, so it is complete now.
            if let (Ok(Some(row)), Some(r)) = (rb.try_read_row(gpu), recorder)
                && let Some(counter) = decode_row(&row, strip)
            {
                r.counter(w.value, counter);
            }
            lock(pool).push(rb);
        }
    }
}

impl Converter for VpConverter {
    fn convert(&self, input: SurfaceId, job: ConvertJob) -> Result<SurfaceInfo, MediaError> {
        if job.format != PixelFormat::Nv12 {
            return Err(MediaError::Unsupported);
        }
        let source = self
            .surfaces
            .capture_texture(input)
            .ok_or(MediaError::InvalidSettings)?;
        let in_size = texture_size(&source);
        let out_size = (job.width & !1, job.height & !1);
        let strip = *lock(&self.strip);
        let mut guard = lock(&self.pipeline);
        let pipeline = self.pipeline_for(&mut guard, in_size, out_size)?;
        let index = pipeline
            .slots
            .iter()
            .position(|s| !s.busy)
            .ok_or(MediaError::OutOfResources)?;
        pipeline
            .vp
            .set_output_colour(job.color.matrix == YCbCrMatrix::Bt601, job.color.full_range);
        let view = pipeline.slots[index].view;
        pipeline
            .vp
            .convert(&self.gpu, &source, view)
            .map_err(|_| MediaError::DeviceLost)?;
        let texture = pipeline.slots[index].texture.clone();
        // Synthetic mode: copy the barcode rows before the signal, so the fence covers them.
        let readback = strip.and_then(|s| {
            let rb = lock(&self.readbacks).pop()?;
            rb.copy_rows(&self.gpu, &texture, s.y);
            Some((rb, s))
        });
        let value = self.next_value.fetch_add(1, Ordering::Relaxed);
        self.fence
            .signal(value)
            .map_err(|_| MediaError::DeviceLost)?;
        self.gpu.flush();
        let h2c = now_ns();
        pipeline.slots[index].busy = true;
        drop(guard);
        let id = self.surfaces.insert(Entry::Converted {
            texture,
            slot: index,
            fence: value,
        });
        if let Some(r) = &self.recorder {
            r.converted(input, id, h2c, value);
        }
        if let Some(tx) = &lock(&self.thread).tx {
            let _ = tx.send(Watch { value, readback });
        }
        Ok(SurfaceInfo {
            id,
            kind: SurfaceKind::D3d11Texture {
                adapter_luid: self.gpu.luid,
            },
            format: PixelFormat::Nv12,
            width: out_size.0,
            height: out_size.1,
            color: job.color,
            capture_ns: h2c,
            source_present_ns: None,
            dirty: None,
            rotation: Rotation::None,
            fence: GpuFence::D3d11Fence {
                shared_handle: 0,
                value,
            },
        })
    }

    fn release(&self, surface: SurfaceId) {
        let Some(entry) = self.surfaces.remove(surface) else {
            return;
        };
        if let Entry::Converted { slot, .. } = &entry {
            let mut guard = lock(&self.pipeline);
            if let Some(s) = guard.as_mut().and_then(|p| p.slots.get_mut(*slot)) {
                s.busy = false;
            }
        }
        if let Some(r) = &self.recorder {
            r.convert_released(surface);
        }
        drop(entry);
    }
}

impl Drop for VpConverter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Decodes a barcode from a luma row.
pub fn decode_row(row: &[u8], strip: Strip) -> Option<u32> {
    barcode::decode(row, strip.x0, strip.width).map(u32::from)
}
