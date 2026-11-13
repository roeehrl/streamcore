//! Fakes for the client stream tests: a decoder that records every call and
//! answers as the test says, the consumer's job list and a lane-0 feedback
//! transport, plus a harness that builds wire messages.
#![allow(dead_code)]

use crate::support::{MS, ManualClock, ManualQueue, lock};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use streamcore_media::binary::Frame;
use streamcore_media::media::{
    ExtendedVideoPacket, FrameVersion, LtrInfo, VideoExtensions, VideoMetadata,
};
use streamcore_media::video_control::{KIND_VIDEO_FEEDBACK, VideoFeedback};
use streamcore_transport::channel::TimedMessage;
use streamcore_video::client::FeedbackTransport;
use streamcore_video::*;

pub const STREAM: u32 = 3;
/// The fake decoder's clock runs this far ahead of the stream's clock.
pub const NATIVE_AHEAD_NS: u64 = 5_000_000_000;
/// Where the test clock starts.
pub const START_NS: u64 = 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Configure { session: usize, config: Vec<u8> },
    Decode(SampleMeta),
    Flush,
    SetMode(PresentMode),
}

/// What the fake does inside `decode` besides answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Inline {
    #[default]
    Nothing,
    /// Report `decoded` and `presented` from inside `decode` (VideoToolbox).
    DecodeAndPresent,
}

pub struct FakeDecoder {
    clock: Arc<ManualClock>,
    pub calls: Mutex<Vec<Call>>,
    /// Every session's port, oldest first.
    pub ports: Mutex<Vec<Arc<dyn DecoderEvents>>>,
    /// Answers for the next `decode` calls (default `Accepted`).
    pub answers: Mutex<VecDeque<DecodeResult>>,
    /// Answers for the next `configure` calls (default `Ok`).
    pub configure_errors: Mutex<VecDeque<MediaError>>,
    pub flush_outcome: Mutex<InvalidateOutcome>,
    pub inline: Mutex<Inline>,
    pub max_queued: Mutex<u8>,
    /// Calls made after the consumer saw `Closed`.
    pub after_close: AtomicUsize,
    pub closed: Arc<AtomicBool>,
    /// Calls made outside a drain.
    pub off_executor: AtomicUsize,
}

impl FakeDecoder {
    pub fn new(clock: Arc<ManualClock>, closed: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            calls: Mutex::default(),
            ports: Mutex::default(),
            answers: Mutex::default(),
            configure_errors: Mutex::default(),
            flush_outcome: Mutex::new(InvalidateOutcome::Completed),
            inline: Mutex::default(),
            max_queued: Mutex::new(1),
            after_close: AtomicUsize::new(0),
            closed,
            off_executor: AtomicUsize::new(0),
        })
    }

    fn note(&self, call: Call) {
        if self.closed.load(Ordering::SeqCst) {
            self.after_close.fetch_add(1, Ordering::SeqCst);
        }
        if !on_executor() {
            self.off_executor.fetch_add(1, Ordering::SeqCst);
        }
        lock(&self.calls).push(call);
    }

    pub fn native_now(&self) -> u64 {
        self.clock.now_ns() + NATIVE_AHEAD_NS
    }

    /// The current session's port.
    pub fn port(&self) -> Arc<dyn DecoderEvents> {
        lock(&self.ports).last().expect("a session").clone()
    }

    pub fn sessions(&self) -> usize {
        lock(&self.ports).len()
    }

    pub fn decoded(&self, seq: u64) {
        self.port().decoded(seq, self.native_now());
    }

    pub fn presented(&self, seq: u64) {
        self.port().presented(seq, self.native_now(), false);
    }

    pub fn decode_seqs(&self) -> Vec<u64> {
        lock(&self.calls)
            .iter()
            .filter_map(|c| match c {
                Call::Decode(meta) => Some(meta.seq),
                _ => None,
            })
            .collect()
    }

    pub fn decode_metas(&self) -> Vec<SampleMeta> {
        lock(&self.calls)
            .iter()
            .filter_map(|c| match c {
                Call::Decode(meta) => Some(*meta),
                _ => None,
            })
            .collect()
    }

    pub fn count(&self, matches: impl Fn(&Call) -> bool) -> usize {
        lock(&self.calls).iter().filter(|c| matches(c)).count()
    }

    pub fn flushes(&self) -> usize {
        self.count(|c| *c == Call::Flush)
    }

    pub fn configures(&self) -> usize {
        self.count(|c| matches!(c, Call::Configure { .. }))
    }

    pub fn answer(&self, result: DecodeResult) {
        lock(&self.answers).push_back(result);
    }
}

impl StreamDecoder for FakeDecoder {
    fn configure(
        &self,
        _codec: Codec,
        config: Vec<u8>,
        _color: ColorTags,
        _queue: Arc<dyn SerialQueue>,
        events: Arc<dyn DecoderEvents>,
    ) -> Result<DecoderInfo, MediaError> {
        let session = self.sessions() + 1;
        self.note(Call::Configure { session, config });
        if let Some(error) = lock(&self.configure_errors).pop_front() {
            return Err(error);
        }
        lock(&self.ports).push(events);
        Ok(DecoderInfo {
            low_latency_confirmed: true,
            hardware: true,
            max_queued: *lock(&self.max_queued),
            notes: vec!["fake".into()],
        })
    }

    fn decode(&self, _sample: Vec<u8>, meta: SampleMeta) -> DecodeResult {
        self.note(Call::Decode(meta));
        let answer = lock(&self.answers)
            .pop_front()
            .unwrap_or(DecodeResult::Accepted);
        if answer == DecodeResult::Accepted && *lock(&self.inline) == Inline::DecodeAndPresent {
            // Synchronous output on the caller's thread, inside decode.
            let port = self.port();
            port.decoded(meta.seq, self.native_now());
            port.presented(meta.seq, self.native_now(), true);
        }
        answer
    }

    fn flush(&self) -> InvalidateOutcome {
        self.note(Call::Flush);
        *lock(&self.flush_outcome)
    }

    fn set_mode(&self, mode: PresentMode) {
        self.note(Call::SetMode(mode));
    }

    fn now_ns(&self) -> u64 {
        self.native_now()
    }
}

/// The consumer's side: every job, in order.
pub struct Jobs {
    pub jobs: Mutex<Vec<ClientJob>>,
    pub closed: Arc<AtomicBool>,
}

impl ClientEvents for Jobs {
    fn post(&self, job: ClientJob) {
        if matches!(job, ClientJob::Closed { .. }) {
            self.closed.store(true, Ordering::SeqCst);
        }
        lock(&self.jobs).push(job);
    }
}

impl Jobs {
    pub fn keyframe_requests(&self) -> Vec<KeyframeReason> {
        lock(&self.jobs)
            .iter()
            .filter_map(|j| match j {
                ClientJob::RequestKeyframe { reason } => Some(*reason),
                _ => None,
            })
            .collect()
    }

    pub fn count(&self, matches: impl Fn(&ClientJob) -> bool) -> usize {
        lock(&self.jobs).iter().filter(|j| matches(j)).count()
    }
}

/// Lane 0 for feedback: every message, decoded.
#[derive(Default)]
pub struct Lane0 {
    pub messages: Mutex<Vec<VideoFeedback>>,
    pub v4: AtomicBool,
}

impl FeedbackTransport for Lane0 {
    fn frames_v4(&self) -> bool {
        self.v4.load(Ordering::SeqCst)
    }
    fn send(&self, message: Vec<u8>) -> bool {
        lock(&self.messages).push(decode_feedback(&message));
        true
    }
}

pub fn decode_feedback(message: &[u8]) -> VideoFeedback {
    let (frame, _) = Frame::read(message).unwrap().unwrap();
    assert_eq!(frame.kind, KIND_VIDEO_FEEDBACK);
    VideoFeedback::decode(frame.payload).unwrap()
}

/// A client stream on a manual queue and clock with the fakes above.
pub struct Client {
    pub clock: Arc<ManualClock>,
    pub queue: Arc<ManualQueue>,
    pub decoder: Arc<FakeDecoder>,
    pub jobs: Arc<Jobs>,
    pub lane0: Arc<Lane0>,
    pub stream: Arc<ClientStream>,
}

impl Client {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(adjust: impl FnOnce(&mut ClientTimings)) -> Self {
        let clock = Arc::new(ManualClock::default());
        clock.advance(START_NS);
        let port = StreamPort::new();
        let queue = ManualQueue::new(port.clone(), clock.clone());
        let closed = Arc::new(AtomicBool::new(false));
        let decoder = FakeDecoder::new(clock.clone(), closed.clone());
        let jobs = Arc::new(Jobs {
            jobs: Mutex::default(),
            closed,
        });
        let lane0 = Arc::new(Lane0::default());
        lane0.v4.store(true, Ordering::SeqCst);
        let mut timings = ClientTimings::default();
        adjust(&mut timings);
        let stream = ClientStream::with_transport(
            ClientParts {
                stream_id: STREAM,
                codec: Codec::H264,
                color: ColorTags::BT709,
                mode: PresentMode::Responsive,
                port,
                queue: queue.clone(),
                decoder: decoder.clone(),
                events: jobs.clone(),
                channel: None,
                timings: Some(timings),
            },
            clock.clone(),
            Some(lane0.clone() as Arc<dyn FeedbackTransport>),
        )
        .unwrap();
        Self {
            clock,
            queue,
            decoder,
            jobs,
            lane0,
            stream,
        }
    }

    pub fn now_us(&self) -> u64 {
        self.clock.now_ns() / 1_000
    }

    /// Deliver a message as the channel would (C1 = now) and drain.
    pub fn deliver(&self, metadata: VideoMetadata, extensions: VideoExtensions, data: &[u8]) {
        let packet = ExtendedVideoPacket {
            metadata,
            extensions,
            data,
        }
        .encode(FrameVersion::V4)
        .unwrap();
        let payload = Frame {
            kind: 0x10,
            payload: &packet,
        }
        .encode()
        .unwrap();
        let now = self.now_us();
        assert!(self.stream.receive(TimedMessage {
            payload,
            first_byte_us: now,
            complete_us: now,
        }));
        self.queue.pump();
    }

    pub fn config(&self, generation: u64) {
        self.deliver(
            VideoMetadata {
                stream_id: STREAM,
                generation,
                sequence: 0,
                flags: 2,
                pts_micros: 0,
            },
            VideoExtensions::default(),
            format!("config-{generation}").as_bytes(),
        );
    }

    pub fn sample(&self, generation: u64, seq: u64, keyframe: bool, extensions: VideoExtensions) {
        self.deliver(
            VideoMetadata {
                stream_id: STREAM,
                generation,
                sequence: seq,
                flags: u8::from(keyframe),
                pts_micros: seq * 16_667,
            },
            extensions,
            &[0, 0, 0, 4, 0x41, 0x9a, seq as u8, 0],
        );
    }

    pub fn key(&self, generation: u64, seq: u64) {
        self.sample(generation, seq, true, VideoExtensions::default());
    }

    pub fn delta(&self, generation: u64, seq: u64) {
        self.sample(generation, seq, false, VideoExtensions::default());
    }

    pub fn discardable(&self, generation: u64, seq: u64) {
        self.sample(
            generation,
            seq,
            false,
            VideoExtensions {
                discardable: true,
                temporal_id: 1,
                ..VideoExtensions::default()
            },
        );
    }

    pub fn ltr(&self, generation: u64, seq: u64, keyframe: bool, token: u32, flags: u8) {
        self.sample(
            generation,
            seq,
            keyframe,
            VideoExtensions {
                ltr: Some(LtrInfo { token, flags }),
                ..VideoExtensions::default()
            },
        );
    }

    /// Decode `seq` (and present it) at the current time, then drain.
    pub fn output(&self, seq: u64) {
        self.decoder.decoded(seq);
        self.decoder.presented(seq);
        self.queue.pump();
    }

    pub fn advance(&self, ns: u64) {
        self.queue.advance(ns);
    }

    pub fn advance_ms(&self, ms: u64) {
        self.queue.advance(ms * MS);
    }

    pub fn feedback(&self) -> Vec<VideoFeedback> {
        lock(&self.lane0.messages).clone()
    }

    pub fn stats(&self) -> ClientStats {
        self.stream.stats()
    }
}
