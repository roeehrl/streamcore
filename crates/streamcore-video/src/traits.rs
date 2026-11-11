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

#[uniffi::export(rust, foreign)]
pub trait CaptureBackend: Send + Sync {
    fn caps(&self) -> CaptureCaps;
    fn enumerate(&self) -> Result<SourceCatalog, MediaError>;
    fn observe_layout(&self, display: u32) -> Option<DisplayLayout>;
    /// Callbacks are delivered on `queue`.
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
}

/// Records GPU work and returns at once.
#[uniffi::export(rust, foreign)]
pub trait Converter: Send + Sync {
    fn convert(&self, input: SurfaceId, job: ConvertJob) -> Result<SurfaceInfo, MediaError>;
    /// Thread-safe, non-blocking leaf call.
    fn release(&self, surface: SurfaceId);
}

/// Draws the latest lease per member into a pooled surface.
#[uniffi::export(rust, foreign)]
pub trait Compositor: Send + Sync {
    /// The pool holds at least permits + 2 surfaces.
    fn configure(&self, plan: CanvasPlan) -> Result<(), MediaError>;
    fn compose(&self, members: Vec<MemberSurface>) -> Result<ComposeResult, MediaError>;
    /// Thread-safe, non-blocking leaf call.
    fn release(&self, surface: SurfaceId);
}

#[uniffi::export(rust, foreign)]
pub trait EncoderFactory: Send + Sync {
    fn probe(&self, adapter: Option<u64>) -> Vec<EncoderCaps>;
    /// `events` is this encoder's port: outputs of an encoder the stream has
    /// replaced are discarded as stale.
    fn create(
        &self,
        spec: EncoderSpec,
        events: Arc<dyn EncoderEvents>,
    ) -> Result<Arc<dyn Encoder>, MediaError>;
}

#[uniffi::export(rust, foreign)]
pub trait Encoder: Send + Sync {
    /// Codec created, hardware, low latency confirmed.
    fn info(&self) -> EncoderInfo;
    /// Never blocks and never replaces the session. Keep `permit` until this
    /// frame's `encoded` or `dropped` has been posted, then drop it; on `Busy` or
    /// `Failed` drop it before returning.
    fn submit(
        &self,
        frame: SurfaceId,
        opts: SubmitOpts,
        permit: Arc<VideoWorkPermit>,
    ) -> SubmitResult;
    fn set_rate(&self, update: RateUpdate) -> Result<(), MediaError>;
    /// The one allowed wait (3.5): completes or discards pending output and
    /// returns within the stream's invalidate bound (500 ms). An adapter whose
    /// platform call has no timeout runs it off the executor and waits with one
    /// ([`crate::run_bounded`]).
    fn invalidate(&self);
}

/// Implemented by Rust; post-only; callable from any thread.
#[uniffi::export]
pub trait EncoderEvents: Send + Sync {
    /// Metadata and payload tokens; the bytes went to the spec's sink first.
    fn encoded(&self, frame: EncodedFrame);
    /// No output for `submit_seq`, including no-output completions.
    fn dropped(&self, submit_seq: u64, reason: DropReason);
}

/// P1's send hop (3.5): the consumer's existing send path. From P2, `VideoSink`
/// replaces it.
#[uniffi::export(rust, foreign)]
pub trait VideoOut: Send + Sync {
    /// Called on the executor with no Rust lock held. The adapter takes its sink
    /// lock (which `revoke` also takes), checks its authority token and calls the
    /// consumer's send. Every completion, a synchronous refusal included, goes to
    /// `receipt.complete`; a receipt dropped without completing counts as refused.
    fn send(&self, meta: VideoOutMeta, bytes: Vec<u8>, receipt: Arc<crate::SinkReceipt>);
    /// Whether the consumer's send path is backpressured; read on the executor
    /// before a recovery keyframe. A leaf call.
    fn backpressured(&self) -> bool;
    /// Revoke the sink's authority: once this returns, `send` enqueues nothing.
    /// A leaf call, made from any thread (by `close` and `revoke`).
    fn revoke(&self);
}

/// The consumer's queue for observer jobs. Non-blocking.
#[uniffi::export(rust, foreign)]
pub trait ObserverQueue: Send + Sync {
    fn post(&self, job: ObserverJob);
}
