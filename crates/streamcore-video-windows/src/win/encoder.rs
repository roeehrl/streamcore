//! `EncoderFactory` and `StreamEncoder` over the Media Foundation hardware H.264 MFT (design doc
//! 4.2): `MFTEnum2` scoped to the stream's adapter LUID, the MFT given the stream's D3D11 device,
//! low-latency CBR with every setting read back, fed only on `METransformNeedInput`.
//!
//! Threads. `create` runs on the executor's bounded helper thread (MFT activation can block).
//! `submit`, `set_rate` and `invalidate` run on the executor. Each session has an event thread
//! that blocks in `GetEvent`: on `NeedInput` it feeds the one held input, if any, and calls
//! `input_ready` after a `Busy`; on `HaveOutput` it calls `ProcessOutput`, splits the parameter
//! sets out, normalises the SPS VUI, builds avcC, converts the access unit to length-prefixed
//! form, hands the bytes to the sink and posts `encoded` at once (R1). It touches no stream state
//! (reentrancy rule 4) and holds no lock while it posts.
//!
//! Failures. A failed event, a failed `ProcessOutput`, a `GetEvent` that keeps failing or a lost
//! device ends the session: the event thread drops every frame still in the encoder (the oldest
//! with `DeviceLost`, or `EncoderFailed { recoverable: true }` so a transient driver error rebuilds
//! the encoder; the stream reacts once) or, with nothing in flight, reports it with
//! `session_failed` (`DeviceLost` non-recoverable), later submits fail with the same error (refused
//! under the book's lock, so no submit can slip in between), and the thread exits. A `ProcessInput` that fails otherwise
//! loses its frame but not its `NeedInput` credit.
//!
//! Teardown. Dropping the encoder hands `END_STREAMING`, `IMFShutdown` and `ShutdownObject` to an
//! `mft-shutdown` thread, which then joins the event thread and releases the MFT last, so the
//! executor never waits on the driver (VIDEO-THREADING "Dropping native objects").
//! `MfEncoderFactory::wait_sessions` waits for those threads (the bench, after close).
//!
//! Fencing. `submit` makes the device wait (on the GPU) for the converter's fence value before
//! `ProcessInput`, so the MFT never reads a half-written NV12 texture; the MFT uses the stream's
//! device through its DXGI device manager, so the wait orders its reads.

use super::surfaces::Surfaces;
use super::sys::com::{Apartment, now_ns};
use super::sys::d3d::{Fence, Gpu};
use super::sys::mf::{self, Encoder, MfEvent, Params, Sample};
use crate::clock::ns_to_hundred_ns;
use crate::h264::annexb::{avcc_config, split_access_unit};
use crate::h264::sps::{self, Colour, Sps};
use crate::mft::{Accept, MftBook, permit_target};
use crate::model::EncoderSetup;
use crate::trace::Recorder;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use streamcore_media::stream::VideoWorkPermit;
use streamcore_video::bitstream::annexb_to_length_prefixed;
use streamcore_video::{
    BitstreamForm, Codec, ColorPrimaries, ColorTags, DropReason, EncodeTimings, EncodedFrame,
    EncoderCaps, EncoderEvents, EncoderFactory, EncoderInfo, EncoderSpec, FeatureSupport,
    InvalidateOutcome, MediaError, PixelFormat, RateUpdate, StreamEncoder, SubmitOpts,
    SubmitResult, SurfaceId, TransferFunction, VideoSinkHandle, YCbCrMatrix, run_bounded,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// H.264 VUI code points (Tables E-3 to E-5) for the stream's output tags.
pub fn h264_colour(c: ColorTags) -> Colour {
    Colour {
        primaries: match c.primaries {
            ColorPrimaries::Bt709 => 1,
            ColorPrimaries::Bt2020 => 9,
            ColorPrimaries::DisplayP3 => 12,
            ColorPrimaries::Unspecified => 2,
        },
        transfer: match c.transfer {
            TransferFunction::Bt709 => 1,
            TransferFunction::Linear => 8,
            TransferFunction::Srgb => 13,
            TransferFunction::Pq => 16,
            TransferFunction::Hlg => 18,
            TransferFunction::Unspecified => 2,
        },
        matrix: match c.matrix {
            YCbCrMatrix::Identity => 0,
            YCbCrMatrix::Bt709 => 1,
            YCbCrMatrix::Bt601 => 6,
            YCbCrMatrix::Bt2020 => 9,
            YCbCrMatrix::Unspecified => 2,
        },
        full_range: c.full_range,
    }
}

/// What the bench reports about the sessions a factory created.
#[derive(Clone, Debug, Default)]
pub struct SessionStats {
    pub setup: Option<EncoderSetup>,
    pub sessions: u64,
    pub inputs_before_first_output: Option<u64>,
    pub max_in_encoder: u64,
    /// The SPS as the encoder wrote it, and whether the normaliser rewrote it.
    pub sps: Option<Sps>,
    pub vui_normalized: bool,
    pub busy: u64,
    pub held: u64,
    pub errors: Vec<String>,
}

pub struct MfEncoderFactory {
    gpu: Arc<Gpu>,
    surfaces: Arc<Surfaces>,
    fence: Arc<Fence>,
    recorder: Option<Arc<Recorder>>,
    /// From the probe; reported in `EncoderCaps`.
    min_inputs_before_output: Option<u64>,
    stats: Arc<Mutex<SessionStats>>,
    /// The `mft-shutdown` threads of dropped sessions.
    closers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl MfEncoderFactory {
    pub fn new(
        gpu: Arc<Gpu>,
        surfaces: Arc<Surfaces>,
        fence: Arc<Fence>,
        recorder: Option<Arc<Recorder>>,
        min_inputs_before_output: Option<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            gpu,
            surfaces,
            fence,
            recorder,
            min_inputs_before_output,
            stats: Arc::default(),
            closers: Arc::default(),
        })
    }

    pub fn stats(&self) -> SessionStats {
        lock(&self.stats).clone()
    }

    /// Waits up to `timeout` until every dropped session is shut down (its MFT shut down, its
    /// event thread joined, its last references released). Blocks: call it on no executor (the
    /// bench calls it after the stream closed). Returns whether all of them finished.
    pub fn wait_sessions(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let (done, left) = {
                let mut closers = lock(&self.closers);
                let (done, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *closers)
                    .into_iter()
                    .partition(|h| h.is_finished());
                *closers = rest;
                (done, closers.len())
            };
            for h in done {
                let _ = h.join();
            }
            if left == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl EncoderFactory for MfEncoderFactory {
    fn probe(&self, adapter: Option<u64>) -> Vec<EncoderCaps> {
        if adapter.is_some_and(|l| l != self.gpu.luid) {
            return Vec::new();
        }
        let _apartment = Apartment::mta();
        let min = self.min_inputs_before_output.unwrap_or(1);
        mf::enumerate_h264(true, Some(self.gpu.luid))
            .unwrap_or_default()
            .into_iter()
            .map(|c| EncoderCaps {
                codec: Codec::H264,
                name: c.info.name,
                hardware: true,
                ltr: FeatureSupport::Unsupported,
                temporal_layers: FeatureSupport::Unsupported,
                intra_refresh: FeatureSupport::Unsupported,
                low_latency_rate_control: FeatureSupport::Requested,
                max_width: 4096,
                max_height: 4096,
                max_fps: 120,
                inputs: vec![PixelFormat::Nv12],
                force_idr: true,
                dynamic_rate: true,
                min_inputs_before_output: u8::try_from(min).unwrap_or(u8::MAX),
                vui_checked: true,
            })
            .collect()
    }

    fn create(
        &self,
        spec: EncoderSpec,
        events: Arc<dyn EncoderEvents>,
    ) -> Result<Arc<dyn StreamEncoder>, MediaError> {
        if spec.codec != Codec::H264 || spec.input_format != PixelFormat::Nv12 {
            return Err(MediaError::Unsupported);
        }
        if spec.adapter.is_some_and(|l| l != self.gpu.luid) {
            return Err(MediaError::Unsupported);
        }
        // The executor's helper thread: join the process MTA for the MFT's lifetime calls.
        let _apartment = Apartment::mta().map_err(|_| MediaError::Internal)?;
        let candidates =
            mf::enumerate_h264(true, Some(self.gpu.luid)).map_err(|_| MediaError::Unsupported)?;
        let candidate = candidates.first().ok_or(MediaError::Unsupported)?;
        let params = Params {
            width: spec.width,
            height: spec.height,
            fps: spec.fps.max(1),
            bitrate: u32::try_from(spec.bitrate_bps).unwrap_or(u32::MAX),
        };
        let encoder = Encoder::create(candidate, &self.gpu, &params).map_err(|e| {
            lock(&self.stats)
                .errors
                .push(format!("create {}: {e}", candidate.info.name));
            MediaError::EncoderFailed { recoverable: false }
        })?;
        let info = EncoderInfo {
            codec: Codec::H264,
            name: encoder.setup.name.clone(),
            hardware: encoder.setup.hardware,
            low_latency: encoder.setup.low_latency_confirmed,
        };
        {
            let mut s = lock(&self.stats);
            s.setup = Some(encoder.setup.clone());
            s.sessions += 1;
        }
        let pump = encoder.event_pump();
        let inner = Arc::new(Inner {
            encoder,
            info,
            events,
            sink: spec.sink,
            colour: h264_colour(spec.color),
            duration_100ns: ns_to_hundred_ns(1_000_000_000 / u64::from(params.fps)),
            surfaces: self.surfaces.clone(),
            fence: self.fence.clone(),
            recorder: self.recorder.clone(),
            stats: self.stats.clone(),
            input: Mutex::new(()),
            book: Mutex::new(MftBook::default()),
            accepted: Mutex::new(HashMap::new()),
            failure: Mutex::new(None),
            wants_ready: AtomicBool::new(false),
            invalidated: AtomicBool::new(false),
            next_token: AtomicU64::new(1),
        });
        let thread_inner = inner.clone();
        let events_thread = std::thread::Builder::new()
            .name("mft-events".into())
            .spawn(move || {
                let _apartment = Apartment::mta();
                event_main(&thread_inner, &pump);
            })
            .map_err(|_| {
                inner.encoder.shutdown();
                MediaError::OutOfResources
            })?;
        Ok(Arc::new(MfEncoder {
            info: inner.info.clone(),
            inner: Some(inner),
            events_thread: Some(events_thread),
            closers: self.closers.clone(),
        }))
    }
}

/// Kept from `submit` until the frame's `encoded` or `dropped` has been posted.
struct Meta {
    seq: u64,
    pts_ns: u64,
    _permit: Arc<VideoWorkPermit>,
}

/// What `ProcessInput` takes, with the ids the recorder needs.
struct Input {
    sample: Sample,
    sample_time: i64,
    surface: SurfaceId,
    seq: u64,
    /// Ask for an IDR right before this input enters the MFT (not before it is held).
    keyframe: bool,
}

struct Inner {
    encoder: Encoder,
    info: EncoderInfo,
    events: Arc<dyn EncoderEvents>,
    sink: Arc<VideoSinkHandle>,
    colour: Colour,
    duration_100ns: i64,
    surfaces: Arc<Surfaces>,
    fence: Arc<Fence>,
    recorder: Option<Arc<Recorder>>,
    stats: Arc<Mutex<SessionStats>>,
    /// Serialises "take a credit, then ProcessInput" between the executor and the event thread,
    /// so inputs enter the MFT in sample-time order. Held across `ProcessInput` only (a native
    /// call that never calls back into Rust).
    input: Mutex<()>,
    book: Mutex<MftBook<Input, Meta>>,
    /// H3 per sequence, for the output's `EncodeTimings`.
    accepted: Mutex<HashMap<u64, u64>>,
    /// Set by a fatal error: every later submit fails with it.
    failure: Mutex<Option<MediaError>>,
    /// A submit was refused with `Busy`: call `input_ready` at the next `NeedInput`.
    wants_ready: AtomicBool,
    invalidated: AtomicBool,
    next_token: AtomicU64,
}

impl Inner {
    /// `ProcessInput` for an input whose credit was taken, with its keyframe request applied
    /// first; returns false on failure (the frame is then dropped). A failure that is not fatal
    /// gives the credit back: the MFT did not take the input and sends no new `NeedInput` for it.
    fn feed(&self, input: &Input, time: i64) -> bool {
        if input.keyframe && self.encoder.force_keyframe().is_err() {
            lock(&self.stats)
                .errors
                .push("AVEncVideoForceKeyFrame refused".into());
        }
        match self.encoder.process_input(&input.sample) {
            Ok(()) => {
                let h3 = now_ns();
                lock(&self.book).input_accepted();
                lock(&self.accepted).insert(input.seq, h3);
                if let Some(r) = &self.recorder {
                    r.submitted(input.surface, input.seq, h3);
                }
                true
            }
            Err(_) if self.encoder.is_closing() || self.invalidated.load(Ordering::Acquire) => {
                // Teardown: the frame's output was being discarded anyway.
                let meta = lock(&self.book).input_failed(time, false);
                if let Some(meta) = meta {
                    self.drop_frame(meta, DropReason::NoOutput);
                }
                false
            }
            Err(e) if mf::is_device_lost(e.code()) => {
                // Take this frame out first, so it is reported even if the session already failed.
                let meta = lock(&self.book).input_failed(time, false);
                self.fail_with(
                    mf::Failure::new(e.code(), format!("ProcessInput: {e}")),
                    meta,
                );
                false
            }
            Err(e) => {
                self.record_error(format!("ProcessInput: {e}"));
                let keep_credit = !mf::is_not_accepting(&e);
                let meta = lock(&self.book).input_failed(time, keep_credit);
                if let Some(meta) = meta {
                    self.drop_frame(
                        meta,
                        DropReason::Error {
                            error: MediaError::EncoderFailed { recoverable: true },
                        },
                    );
                }
                false
            }
        }
    }

    /// Posts `dropped`, then releases the frame's permit.
    fn drop_frame(&self, meta: Meta, reason: DropReason) {
        if let Some(r) = &self.recorder {
            r.encoder_dropped();
        }
        self.events.dropped(meta.seq, reason);
        drop(meta);
    }

    fn token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed)
    }

    fn on_output(&self, out: mf::Output) {
        let (meta, skipped) = lock(&self.book).output(out.time_100ns);
        for m in skipped {
            self.drop_frame(m, DropReason::NoOutput);
        }
        let Some(meta) = meta else {
            lock(&self.stats)
                .errors
                .push(format!("output with unknown time {}", out.time_100ns));
            return;
        };
        let h3 = lock(&self.accepted).remove(&meta.seq);
        let split = split_access_unit(&out.bytes);
        let keyframe = out.keyframe || split.idr;
        let mut config_token = None;
        // An encoder that keeps SPS and PPS out of its keyframes has them in the output type.
        let header = (keyframe && (split.sps.is_none() || split.pps.is_none()))
            .then(|| self.encoder.sequence_header())
            .flatten()
            .map(|h| split_access_unit(&h));
        let sps_nal = split
            .sps
            .as_ref()
            .or(header.as_ref().and_then(|h| h.sps.as_ref()));
        let pps = split
            .pps
            .as_ref()
            .or(header.as_ref().and_then(|h| h.pps.as_ref()));
        if let (Some(sps_nal), Some(pps)) = (sps_nal, pps) {
            let normalized = sps::normalize(sps_nal, self.colour);
            {
                let mut s = lock(&self.stats);
                if s.sps.is_none() {
                    s.sps = sps::parse(sps_nal);
                    s.vui_normalized = normalized.as_deref().is_some_and(|n| n != &sps_nal[..]);
                }
            }
            let sps_out = normalized.unwrap_or_else(|| sps_nal.clone());
            if let Some(config) = avcc_config(&sps_out, pps) {
                let token = self.token();
                if self.sink.submit_encoded(token, config) {
                    config_token = Some(token);
                }
            }
        }
        let Some(payload) = annexb_to_length_prefixed(split.annexb) else {
            self.drop_frame(meta, DropReason::NoOutput);
            return;
        };
        let bytes = payload.len();
        let payload_token = self.token();
        if !self.sink.submit_encoded(payload_token, payload) {
            self.drop_frame(meta, DropReason::NoOutput);
            return;
        }
        if let Some(r) = &self.recorder {
            r.encoded(meta.seq, out.h4_ns, bytes, keyframe);
        }
        self.events.encoded(EncodedFrame {
            submit_seq: meta.seq,
            pts_ns: meta.pts_ns,
            keyframe,
            config_token,
            payload_token,
            form: BitstreamForm::Avcc4,
            discardable: false,
            temporal_id: None,
            ltr: None,
            timings: EncodeTimings {
                input_ns: h3,
                output_ns: Some(out.h4_ns),
            },
        });
        drop(meta);
    }

    fn on_need_input(&self) {
        let guard = lock(&self.input);
        let held = lock(&self.book).need_input();
        if let Some(input) = held {
            self.feed(&input, input.sample_time);
        }
        drop(guard);
        if self.wants_ready.swap(false, Ordering::AcqRel)
            && !self.invalidated.load(Ordering::Acquire)
        {
            self.events.input_ready();
        }
    }

    fn record_error(&self, error: String) {
        let mut s = lock(&self.stats);
        if s.errors.len() < 50 {
            s.errors.push(error);
        }
    }

    /// A session-ending MFT error. Later submits fail with it; every frame in the encoder is
    /// dropped, the oldest with the error (`DeviceLost` or `EncoderFailed { recoverable: true }`)
    /// and the rest as no-output; with nothing in flight, `session_failed` reports it
    /// (`DeviceLost` as non-recoverable), so a still screen learns of it without a submit.
    fn fail(&self, failure: mf::Failure) {
        self.fail_with(failure, None);
    }

    /// `fail`, with `first` (a frame already taken out of the book) reported first.
    fn fail_with(&self, failure: mf::Failure, first: Option<Meta>) {
        let device_lost = failure.device_lost;
        self.record_error(failure.message);
        let error = if device_lost {
            MediaError::DeviceLost
        } else {
            MediaError::EncoderFailed { recoverable: true }
        };
        // The error is stored before the book refuses submits, so a refused submit can read it.
        lock(&self.failure).get_or_insert_with(|| error.clone());
        let taken = lock(&self.book).fail();
        if taken.is_none() && first.is_none() {
            return;
        }
        let metas: Vec<Meta> = first.into_iter().chain(taken.unwrap_or_default()).collect();
        lock(&self.accepted).clear();
        // Decided from what `book.fail()` took under the book's lock: a submit racing this either
        // got in first (and is dropped here with the error) or is refused with `Failed`.
        let reported = !metas.is_empty();
        let mut reason = Some(DropReason::Error {
            error: error.clone(),
        });
        for m in metas {
            self.drop_frame(m, reason.take().unwrap_or(DropReason::NoOutput));
        }
        if !reported && !self.invalidated.load(Ordering::Acquire) {
            self.events.session_failed(error, !device_lost);
        }
    }

    /// Every frame still in the encoder: posted as dropped, permits released.
    fn abandon_all(&self) {
        let metas = lock(&self.book).take_all();
        lock(&self.accepted).clear();
        for m in metas {
            self.drop_frame(m, DropReason::NoOutput);
        }
    }
}

fn event_main(inner: &Inner, pump: &mf::EventPump) {
    loop {
        match pump.next() {
            MfEvent::NeedInput => inner.on_need_input(),
            MfEvent::Output(o) => inner.on_output(o),
            MfEvent::DrainComplete | MfEvent::NoOutput | MfEvent::Other(_) => {}
            MfEvent::Failed(f) => {
                inner.fail(f);
                break;
            }
            MfEvent::Shutdown => break,
        }
    }
    inner.abandon_all();
    let book = lock(&inner.book);
    let mut s = lock(&inner.stats);
    s.inputs_before_first_output = s
        .inputs_before_first_output
        .or(book.inputs_before_first_output());
    s.max_in_encoder = s.max_in_encoder.max(book.max_in_encoder() as u64);
}

/// The handle the stream holds. Dropping it hands the MFT's shutdown to an `mft-shutdown`
/// thread; the event thread then sees `MF_E_SHUTDOWN`, abandons what is left and exits, and the
/// shutdown thread joins it and releases the session last.
pub struct MfEncoder {
    info: EncoderInfo,
    /// `None` only once `drop` has handed the session to its shutdown thread.
    inner: Option<Arc<Inner>>,
    events_thread: Option<JoinHandle<()>>,
    closers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

type Job = Box<dyn FnOnce() + Send>;

impl Drop for MfEncoder {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        inner.invalidated.store(true, Ordering::Release);
        let events_thread = self.events_thread.take();
        let close: Job = Box::new(move || {
            let _apartment = Apartment::mta();
            inner.encoder.shutdown();
            if let Some(t) = events_thread {
                let _ = t.join();
            }
            // The last references to the MFT and its device manager go here, off the executor.
            drop(inner);
        });
        let job = Arc::new(Mutex::new(Some(close)));
        let thread_job = job.clone();
        let spawned = std::thread::Builder::new()
            .name("mft-shutdown".into())
            .spawn(move || {
                let close = lock(&thread_job).take();
                if let Some(close) = close {
                    close();
                }
            });
        match spawned {
            Ok(handle) => {
                // A long-lived host creates many encoders: keep only the closers still running.
                let mut closers = lock(&self.closers);
                closers.retain(|h| !h.is_finished());
                closers.push(handle);
            }
            Err(_) => {
                // No thread to be had: shutting down here is the only way not to leak the session.
                let close = lock(&job).take();
                if let Some(close) = close {
                    close();
                }
            }
        }
    }
}

impl StreamEncoder for MfEncoder {
    fn info(&self) -> EncoderInfo {
        self.info.clone()
    }

    fn submit(
        &self,
        frame: SurfaceId,
        opts: SubmitOpts,
        permit: Arc<VideoWorkPermit>,
    ) -> SubmitResult {
        let Some(inner) = &self.inner else {
            return SubmitResult::Failed {
                error: MediaError::Internal,
            };
        };
        if let Some(error) = lock(&inner.failure).clone() {
            return SubmitResult::Failed { error };
        }
        if inner.invalidated.load(Ordering::Acquire) {
            return SubmitResult::Failed {
                error: MediaError::Internal,
            };
        }
        let Some((texture, fence)) = inner.surfaces.converted(frame) else {
            return SubmitResult::Failed {
                error: MediaError::InvalidSettings,
            };
        };
        // The MFT reads the texture on this device: order that after the convert on the GPU.
        if inner.fence.gpu_wait(fence).is_err() {
            return SubmitResult::Failed {
                error: MediaError::DeviceLost,
            };
        }
        let time = ns_to_hundred_ns(opts.pts_ns);
        let sample = match Encoder::sample(&texture, time, inner.duration_100ns) {
            Ok(s) => s,
            Err(_) => {
                return SubmitResult::Failed {
                    error: MediaError::OutOfResources,
                };
            }
        };
        // The keyframe request travels with the input: applied when it is fed (now, or from
        // NeedInput if held), never for a `Busy` refusal, whose retry asks again.
        let input = Input {
            sample,
            sample_time: time,
            surface: frame,
            seq: opts.submit_seq,
            keyframe: opts.force_keyframe,
        };
        let meta = Meta {
            seq: opts.submit_seq,
            pts_ns: opts.pts_ns,
            _permit: permit,
        };
        let _order = lock(&inner.input);
        let (accept, refused) = lock(&inner.book).submit(time, input, meta);
        match accept {
            Accept::Now(input) => {
                // On failure the frame's `dropped` was posted; the sequence is consumed either way.
                inner.feed(&input, time);
                SubmitResult::Submitted
            }
            Accept::Held => {
                lock(&inner.stats).held += 1;
                SubmitResult::Submitted
            }
            Accept::Failed(_) => {
                drop(refused);
                let error = lock(&inner.failure)
                    .clone()
                    .unwrap_or(MediaError::EncoderFailed { recoverable: true });
                SubmitResult::Failed { error }
            }
            Accept::Busy(_) => {
                drop(refused);
                lock(&inner.stats).busy += 1;
                inner.wants_ready.store(true, Ordering::Release);
                SubmitResult::Busy
            }
        }
    }

    fn set_rate(&self, update: RateUpdate) -> Result<(), MediaError> {
        let inner = self.inner.as_ref().ok_or(MediaError::Internal)?;
        inner
            .encoder
            .set_bitrate(u32::try_from(update.bitrate_bps).unwrap_or(u32::MAX))
            .map_err(|_| MediaError::Unsupported)
    }

    fn invalidate(&self) -> InvalidateOutcome {
        // Discard pending output (`COMMAND_FLUSH`), bounded: a flush that hangs is left on its
        // helper thread, and this session's frames are dropped (their permits released) now, so
        // a hung flush does not hold admission. `TimedOut` tells the stream the session hung.
        let Some(inner) = &self.inner else {
            return InvalidateOutcome::Completed;
        };
        inner.invalidated.store(true, Ordering::Release);
        let helper = inner.clone();
        let finished = run_bounded(Duration::from_millis(450), move || {
            let _apartment = Apartment::mta();
            let _ = helper.encoder.flush();
            helper.abandon_all();
        });
        if finished {
            return InvalidateOutcome::Completed;
        }
        inner.record_error("MFT flush did not finish within 450 ms".into());
        inner.abandon_all();
        InvalidateOutcome::TimedOut
    }
}

/// The permit target for the probe's `min_inputs_before_output`.
pub fn plan_permits(min_inputs_before_output: Option<u64>) -> u8 {
    permit_target(min_inputs_before_output)
}
