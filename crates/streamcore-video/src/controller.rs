//! `StreamController`: the consumer's handle on one stream. Every method posts
//! to the stream's executor and returns at once, except `close_and_wait`.

use crate::error::MediaError;
use crate::executor::*;
use crate::sink::{SinkState, VideoOutput, VideoSink};
use crate::stream::{Backends, Command, StreamState, validate};
use crate::traits::*;
use crate::types::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Duration;

/// Everything a stream needs. `port` must be the port `queue` calls; it is
/// bound to this stream and cannot be reused.
#[derive(uniffi::Record)]
pub struct StreamParts {
    pub stream_id: u32,
    pub spec: CaptureSpec,
    pub plan: StreamPlan,
    pub port: Arc<StreamPort>,
    pub queue: Arc<dyn SerialQueue>,
    pub capture: Arc<dyn CaptureBackend>,
    /// Required when `plan.convert` is `Convert`.
    pub converter: Option<Arc<dyn Converter>>,
    pub encoders: Arc<dyn EncoderFactory>,
    /// Where encoded video goes: the stream's `VideoSink` sends it on this
    /// output's channel (3.4).
    pub output: Arc<VideoOutput>,
    pub observer: Arc<dyn ObserverQueue>,
    pub timings: Option<StreamTimings>,
}

/// One stream: a serial executor over the backends in [`StreamParts`].
/// Dropping it without closing posts a close; the executor then keeps itself
/// alive until `Closed`.
#[derive(uniffi::Object)]
pub struct StreamController {
    shared: Arc<Shared>,
}

#[uniffi::export]
impl StreamController {
    /// Validates the spec and plan before anything native is allocated.
    #[uniffi::constructor]
    pub fn new(parts: StreamParts) -> Result<Arc<Self>, MediaError> {
        Self::with_clock(parts, Arc::new(MonotonicClock::default()))
    }

    /// Start capture and encoding. The result is an `OpResult` for the returned
    /// token on the observer queue; `Started` precedes a successful one.
    pub fn start(&self) -> OpToken {
        let op = self.next_command();
        self.shared.post(Event::Command(Command::Start(op)));
        op
    }

    /// Rate, capture parameters or encode size.
    pub fn update(&self, update: StreamUpdate) -> OpToken {
        let op = self.next_command();
        self.shared
            .post(Event::Command(Command::Update(op, update)));
        op
    }

    /// Stop the capture; the stream can be started again.
    pub fn stop(&self) -> OpToken {
        let op = self.next_command();
        self.shared.post(Event::Command(Command::Stop(op)));
        op
    }

    /// A client keyframe request (recovers at once, fenced at the latest
    /// submission).
    pub fn request_keyframe(&self) {
        self.shared.post(Event::Command(Command::RequestKeyframe));
    }

    /// The source display's refresh interval changed (a display mode change);
    /// `None` when unknown, which aligns admission to the target interval
    /// instead (6.3). The initial value is `StreamPlan::refresh_interval_ns`.
    pub fn set_refresh_interval(&self, refresh_interval_ns: Option<u64>) {
        self.shared.post(Event::Command(Command::Refresh(
            crate::stream::valid_refresh(refresh_interval_ns),
        )));
    }

    /// The client reported flushing `frames` frames (skip cause e,
    /// `StatsSnapshot::client_flushed`).
    pub fn note_client_flush(&self, frames: u64) {
        self.shared
            .post(Event::Command(Command::ClientFlush(frames)));
    }

    /// Post a `Stats` job to the observer queue.
    pub fn request_stats(&self) {
        self.shared.post(Event::Command(Command::Stats));
    }

    /// Revoke the stream's screen authority: once this returns, the sink
    /// enqueues nothing more for it, and the executor makes no further submit or
    /// send.
    pub fn revoke(&self) {
        self.shared.revoke();
        self.shared.post(Event::Command(Command::Revoked));
    }

    /// Revoke the sink, post `Close` and return at once. The completion is an
    /// `ObserverJob::Closed` on the observer queue.
    pub fn begin_close(&self) {
        if self.shared.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.revoke();
        self.shared.post(Event::Close { keepalive: None });
    }

    /// `begin_close`, then wait without blocking for the close to complete.
    pub async fn close(&self) -> CloseOutcome {
        self.begin_close();
        CloseFuture {
            shared: self.shared.clone(),
        }
        .await
    }

    /// For synchronous native teardown only. On an executor or a
    /// `streamcore-network` worker it asserts in debug builds and returns
    /// `Internal` in release builds. Elsewhere it waits at most `timeout_ms`
    /// and returns `Closed`, or `StopUnknown` (also on timeout); the sink is
    /// already revoked.
    pub fn close_and_wait(&self, timeout_ms: u64) -> Result<CloseOutcome, MediaError> {
        if !blocking_allowed() {
            debug_assert!(
                false,
                "close_and_wait on a stream executor or a network worker (rule 6)"
            );
            return Err(MediaError::Internal);
        }
        self.begin_close();
        Ok(self
            .shared
            .close_slot
            .wait(Duration::from_millis(timeout_ms))
            .unwrap_or(CloseOutcome::StopUnknown))
    }

    /// The phase the executor last published.
    pub fn phase(&self) -> StreamPhase {
        lock(&self.shared.published).phase
    }

    /// The counters the executor last published.
    pub fn stats(&self) -> StatsSnapshot {
        lock(&self.shared.published).stats
    }

    /// The sink's current backlog (6.2). A leaf call, any thread.
    pub fn sink_state(&self) -> SinkState {
        self.shared.sink.state()
    }
}

impl StreamController {
    /// [`StreamController::new`] with an injected clock (tests, simulations).
    pub fn with_clock(parts: StreamParts, clock: Arc<dyn Clock>) -> Result<Arc<Self>, MediaError> {
        validate(&parts.spec, &parts.plan)?;
        if parts.plan.convert == ConvertPath::Convert && parts.converter.is_none() {
            return Err(MediaError::InvalidSettings);
        }
        let timings = parts.timings.unwrap_or_default();
        let StreamParts {
            stream_id,
            spec,
            plan,
            port,
            queue,
            capture,
            converter,
            encoders,
            output,
            observer,
            ..
        } = parts;
        let backends = Backends {
            capture,
            converter,
            encoders,
            observer,
        };
        let sink = VideoSink::new(&output, clock.clone(), stream_id, &plan);
        let shared = Shared::new(queue, sink, clock, |weak| {
            StreamState::new(weak.clone(), stream_id, spec, plan, timings, backends)
        });
        shared.sink.bind(Arc::downgrade(&shared));
        port.bind(&shared)?;
        Ok(Arc::new(Self { shared }))
    }

    fn next_command(&self) -> OpToken {
        OpToken {
            id: self.shared.next_command.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
        }
    }

    /// The close outcome, once the close completed.
    pub fn close_outcome(&self) -> Option<CloseOutcome> {
        self.shared.close_slot.outcome()
    }
}

impl Drop for StreamController {
    fn drop(&mut self) {
        if self.shared.close_slot.outcome().is_some() {
            return;
        }
        self.shared.closing.store(true, Ordering::Release);
        self.shared.revoke();
        // The event keeps the executor alive until it reaches Closed.
        self.shared.post(Event::Close {
            keepalive: Some(self.shared.clone()),
        });
    }
}

struct CloseFuture {
    shared: Arc<Shared>,
}

impl Future for CloseFuture {
    type Output = CloseOutcome;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<CloseOutcome> {
        match self.shared.close_slot.poll(cx.waker()) {
            Some(outcome) => Poll::Ready(outcome),
            None => Poll::Pending,
        }
    }
}
