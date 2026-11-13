//! The backend traits (3.4). Host traits are UniFFI foreign traits (Swift and
//! Kotlin implement them); Windows and Linux implement them in Rust.
//!
//! Every outbound method here except `release` runs only on the stream executor,
//! during a drain ([`crate::threading`]). Every `*Events` method only posts and
//! returns in O(1).

use crate::error::MediaError;
use crate::types::*;
use std::sync::Arc;
use streamcore_media::stream::VideoWorkPermit;

/// Where a stream's executor runs. Implemented natively on Apple and Android, in
/// Rust elsewhere. The queue calls [`crate::StreamPort::run`] for a wake and
/// [`crate::StreamPort::run_timer`] for a timer. Whether the current thread is in
/// a drain is tracked in Rust.
#[uniffi::export(rust, foreign)]
pub trait SerialQueue: Send + Sync {
    /// Run the port's `run()` on this queue soon; never inline. Neither blocks
    /// nor takes an app lock.
    fn wake(&self);
    /// Run the port's `run_timer(timer)` on this queue after `delay_ns`. Neither
    /// blocks nor takes an app lock.
    fn wake_after(&self, delay_ns: u64, timer: TimerToken);
}

/// Thread guarantee: `caps`, `open`, `start`, `update` and `stop` are called
/// only by the executor during a drain that the stream's queue started
/// (`StreamPort::run` or `run_timer`), so on the queue's own thread, and never
/// inside an inline frame drain (which runs on the thread that called
/// `CaptureEvents::frame`). A backend that must create thread-affine objects on
/// the queue (a WGC frame pool bound to the queue's `DispatcherQueue`) can do so
/// in `open` or `start`. `release` alone may run on any thread.
#[uniffi::export(rust, foreign)]
pub trait CaptureBackend: Send + Sync {
    /// Read by the executor when a stream starts.
    fn caps(&self) -> CaptureCaps;
    fn enumerate(&self) -> Result<SourceCatalog, MediaError>;
    fn observe_layout(&self, display: u32) -> Option<DisplayLayout>;
    /// Callbacks are delivered on `queue`: `frame` may drain the executor
    /// inline on the calling thread, so a backend that delivers frames on
    /// another thread (a free-threaded pool) runs convert and submit there.
    fn open(
        &self,
        spec: CaptureSpec,
        queue: Arc<dyn SerialQueue>,
        events: Arc<dyn CaptureEvents>,
    ) -> Result<CaptureId, MediaError>;
    /// The result arrives through `events.op_done(op, ..)`, always posted, even
    /// when it is known at once.
    fn start(&self, id: CaptureId, op: OpToken);
    /// Frame interval, live region origin, cursor. Result via `op_done`.
    fn update(&self, id: CaptureId, update: CaptureUpdate, op: OpToken);
    /// Result via `op_done`. Every lease is cleared before this is called.
    fn stop(&self, id: CaptureId, op: OpToken);
    /// Thread-safe, non-blocking leaf call; a late release (old slot
    /// generation) is a no-op.
    fn release(&self, surface: SurfaceId);
}

/// Implemented by Rust; post-only.
#[uniffi::export]
pub trait CaptureEvents: Send + Sync {
    /// A new image (`status` Complete) or a status change. The surface is leased
    /// to Rust from this call on, also when the stream is gone (it is then
    /// released on the calling thread).
    fn frame(&self, id: CaptureId, member: Option<u8>, surface: SurfaceInfo, status: FrameStatus);
    /// `error` is `None` on success.
    fn op_done(&self, op: OpToken, error: Option<MediaError>);
    fn source_gone(&self, id: CaptureId, reason: SourceGone);
    /// The source's size, scale or origin changed (display mode, DPI, rotation,
    /// window resize). Posts `ObserverJob::LayoutChanged`; the consumer re-plans.
    fn layout_changed(&self, id: CaptureId, layout: DisplayLayout);
    /// The backend dropped `count` frames unseen (skip cause (a), 6.7): a pool
    /// backlog where it kept only the newest. A leaf call: it counts into
    /// `StatsSnapshot::capture_skipped` and posts nothing.
    fn skipped(&self, id: CaptureId, member: Option<u8>, count: u32);
}

/// Records GPU work and returns at once.
#[uniffi::export(rust, foreign)]
pub trait Converter: Send + Sync {
    /// Rust's hold on `input` for this frame ends when `convert` returns: the
    /// encoder reads only the output, so the capture surface is not pinned
    /// until the encoder's output (the latest slot may still hold it). A
    /// converter whose recorded GPU work still reads `input` keeps it valid
    /// until that work completes (its fence): it holds its own reference to
    /// the native surface (a retained `CVPixelBuffer`), or defers the
    /// producer's reuse to the fence through a slot table it shares with the
    /// capture backend (a WGC frame's `Close`).
    fn convert(&self, input: SurfaceId, job: ConvertJob) -> Result<SurfaceInfo, MediaError>;
    /// Thread-safe, non-blocking leaf call.
    fn release(&self, surface: SurfaceId);
}

/// Draws the latest lease per member into a pooled surface (3.9). Optional: a
/// backend without one has no canvas ([`crate::StreamController::new`] refuses
/// a `Canvas` target as `Unsupported`), and a consumer passes one to
/// [`crate::StreamController::new_canvas`]. Rust keeps every policy (layout,
/// tiles, group lifecycle, deadlines, cadence, input mapping); the compositor
/// only draws.
#[uniffi::export(rust, foreign)]
pub trait Compositor: Send + Sync {
    /// Called on every start, before any member capture opens. The pool holds
    /// at least `plan.pool_size` (permits + 2) surfaces of the plan's size.
    fn configure(&self, plan: CanvasPlan) -> Result<(), MediaError>;
    /// Draw each member's surface into its tile (upright: a surface whose
    /// `SurfaceInfo::rotation` is not `None` is turned by it; scaled to fit;
    /// the member's colour converted to `plan.color`) and every gap black;
    /// records GPU work
    /// and returns at once, like `convert`: Rust's hold on the member surfaces
    /// ends when this returns, so GPU work that still reads one keeps it valid
    /// itself. The composite is leased to Rust until `release`; its `fence`
    /// says when the drawing is complete. `PoolExhausted` (or
    /// `OutOfResources`) while every pool surface is leased: Rust retries when
    /// it releases one.
    fn compose(&self, members: Vec<MemberSurface>) -> Result<ComposeResult, MediaError>;
    /// Thread-safe, non-blocking leaf call.
    fn release(&self, surface: SurfaceId);
}

#[uniffi::export(rust, foreign)]
pub trait EncoderFactory: Send + Sync {
    fn probe(&self, adapter: Option<u64>) -> Vec<EncoderCaps>;
    /// `events` is this encoder's port: outputs of an encoder the stream has
    /// replaced are discarded as stale. May block (MFT activation, an NVENC
    /// open): the executor runs it on a helper thread and waits at most
    /// `StreamTimings::create_bound_ns`; a session that arrives later is
    /// invalidated and dropped on that thread.
    fn create(
        &self,
        spec: EncoderSpec,
        events: Arc<dyn EncoderEvents>,
    ) -> Result<Arc<dyn StreamEncoder>, MediaError>;
}

/// The design's `Encoder` (3.4); [`crate::Encoder`] is an alias. Named
/// `StreamEncoder` because Swift's standard library already has an `Encoder`
/// (Codable), and UniFFI 0.32.2 cannot rename a foreign trait in Swift only (the
/// rename also changes an FFI symbol, which then fails to link).
#[uniffi::export(rust, foreign)]
pub trait StreamEncoder: Send + Sync {
    /// Codec created, hardware, low latency confirmed.
    fn info(&self) -> EncoderInfo;
    /// Never blocks and never replaces the session. Keep `permit` until this
    /// frame's `encoded` or `dropped` has been posted, then drop it; on `Busy` or
    /// `Failed` drop it before returning. After `Busy`, call
    /// `EncoderEvents::input_ready` once the encoder can take input again (MF's
    /// `METransformNeedInput`): the retained frame is then submitted even if no
    /// new capture arrives.
    fn submit(
        &self,
        frame: SurfaceId,
        opts: SubmitOpts,
        permit: Arc<VideoWorkPermit>,
    ) -> SubmitResult;
    /// A live rate change (a consumer update, or rate adaptation, 6.6): never
    /// replaces the session. Apply `bitrate_bps`. `fps` is a hint: an adapter
    /// may ignore it, because the pacer enforces the frame rate by admitting
    /// fewer frames, and rate adaptation never depends on the encoder for it.
    /// `rate_window_ms` and `max_frame_qp` are optional (`None`: leave as is);
    /// an encoder without them ignores them, and `max_frame_qp` is sent only
    /// when [`EncoderCaps::max_frame_qp`] confirms it. A resolution change is
    /// never a rate update: it creates a new session (a new generation).
    fn set_rate(&self, update: RateUpdate) -> Result<(), MediaError>;
    /// The one allowed wait (3.5): completes or discards pending output and
    /// returns within the stream's invalidate bound (500 ms). An adapter whose
    /// platform call has no timeout runs it off the executor, waits with a bound
    /// of its own below the stream's ([`crate::run_bounded`], 450 ms in the
    /// shipped adapters) and returns [`InvalidateOutcome::TimedOut`] when that
    /// bound passes. On `TimedOut`, or when the call itself overran the stream's
    /// bound (a backstop), the executor treats the session as hung: it stops
    /// counting the session's frames against admission (releases their
    /// permits), counts `invalidate_timeouts` and posts `EncoderFailed`. Outputs
    /// the session produces later carry its old generation and are discarded.
    fn invalidate(&self) -> InvalidateOutcome;
}

/// Implemented by Rust; post-only; callable from any thread. Outputs must be
/// length-prefixed (`BitstreamForm::Avcc4`, avcC/hvcC config): an adapter whose
/// encoder emits Annex-B converts on its output thread
/// ([`crate::bitstream::annexb_to_length_prefixed`]); the executor fails the
/// stream on an Annex-B output.
#[uniffi::export]
pub trait EncoderEvents: Send + Sync {
    /// Metadata and payload tokens; the bytes went to the spec's sink first.
    fn encoded(&self, frame: EncodedFrame);
    /// No output for `submit_seq`, including no-output completions.
    fn dropped(&self, submit_seq: u64, reason: DropReason);
    /// The encoder can take input again after a `Busy` submit.
    fn input_ready(&self);
    /// The session failed on its own, outside any `submit` (a lost device, a
    /// fatal driver error), typically with no frame in flight, so no `dropped`
    /// can carry the error: on a still screen the stream would otherwise learn
    /// of it only at the next capture. A leaf call like the others: it posts
    /// and returns, and carries this port's encoder generation (a call from a
    /// replaced session is stale and ignored). The executor keys on the error
    /// type, as for a dropped frame. `DeviceLost` always rebuilds the encoder,
    /// whatever `recoverable` says, exactly as a frame dropped with `DeviceLost`
    /// does. Another error with `recoverable` (and not
    /// `EncoderFailed { recoverable: false }`) reports
    /// `ObserverJob::EncoderFailed` and rebuilds. Anything else fails the stream
    /// with `error`'s wire code. Every such rebuild counts against the stream's
    /// rebuild budget (more than 3 in 10 s fail the stream; see
    /// [`crate::threading`]). With nothing in flight no loss is recorded (the
    /// rebuild's IDR restarts the stream); frames the session still held are
    /// counted lost. Drop the session's frames first (their drops record the
    /// loss), then call this once: a session's failure rebuilds and counts once,
    /// so a call after a `DeviceLost` drop of the same session is a no-op.
    fn session_failed(&self, error: MediaError, recoverable: bool);
}

/// The consumer's queue for observer jobs. Non-blocking.
#[uniffi::export(rust, foreign)]
pub trait ObserverQueue: Send + Sync {
    fn post(&self, job: ObserverJob);
}
