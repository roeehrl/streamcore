//! A platform-neutral video host for client bring-up (P4): a real
//! `StreamController` sending through `VideoSink` on a `SecureChannel`, with a
//! timer-driven capture and a synthetic encoder that loops a short pre-encoded
//! H.264 clip (`examples/data/pattern-640x360.h264`, see `examples/data/README.md`).
//! No platform capture or encoder is involved, so it runs anywhere the
//! transport does (macOS, Windows, Linux).
//!
//! ```text
//! cargo run --release -p streamcore-video --example video_host -- --listen <lan-ip>:7700 [--pair-port 7701] [--fps 60]
//! ```
//!
//! It prints one connect line per pairing window (120 s, then a new PIN):
//!
//! ```text
//! streamcore-video://<lan-ip>:7700?pair=7701&pin=123456&fp=<host certificate sha-256>
//! ```
//!
//! A client pairs with the PIN on the pairing port (`pair_with_host`, checking
//! the fingerprint), connects with its pinned identity on the stream port
//! (TLS/TCP or QUIC) advertising `streamcore.video-demo.v1` and `frames.v4`,
//! and receives stream 1 on lane 1. It may send `{"type":"keyframe"}` as an
//! envelope kind `0x01` message: the host requests a keyframe, and the
//! synthetic encoder jumps to the clip's next IDR. `videoFeedback` from the
//! client feeds the stream's client-delay inputs; the host prints stats every
//! 5 s. One client at a time: a new connection replaces the stream.

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use streamcore_media::binary::Frame;
use streamcore_media::media::FRAMES_V4_CAPABILITY;
use streamcore_media::stream::VideoWorkPermit;
use streamcore_media::video_control::{KIND_VIDEO_FEEDBACK, VideoFeedback};
use streamcore_transport::channel::{
    ChannelTransport, SecureChannel, SessionOptions, listen_secure,
};
use streamcore_transport::identity::PeerIdentity;
use streamcore_transport::pairing_transport::listen_pairing;
use streamcore_video::*;

/// The demo's own Hello capability: a client must advertise it.
const DEMO_CAPABILITY: &str = "streamcore.video-demo.v1";
const STREAM_ID: u32 = 1;
const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const CLIP: &[u8] = include_bytes!("data/pattern-640x360.h264");
const PAIRING_WINDOW: Duration = Duration::from_secs(120);

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec![DEMO_CAPABILITY.into(), FRAMES_V4_CAPABILITY.into()],
        required: vec![DEMO_CAPABILITY.into()],
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Host-monotonic nanoseconds (H1) from a process-wide anchor.
fn host_ns() -> u64 {
    static ANCHOR: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    u64::try_from(ANCHOR.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// The clip.

/// One access unit of the clip, length-prefixed (`Avcc4`), parameter sets
/// removed (they travel in the avcC configuration).
struct AccessUnit {
    idr: bool,
    sample: Vec<u8>,
}

struct Clip {
    units: Vec<AccessUnit>,
    /// `AVCDecoderConfigurationRecord` from the clip's SPS and PPS.
    avcc: Vec<u8>,
}

impl Clip {
    /// Split the Annex-B clip into access units (one slice per picture: the
    /// clip is encoded with one thread) and build its avcC.
    fn parse(annexb: &[u8]) -> Clip {
        let mut nals = Vec::new();
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= annexb.len() {
            if annexb[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        for (n, &start) in starts.iter().enumerate() {
            let mut end = starts.get(n + 1).map_or(annexb.len(), |next| next - 3);
            while end > start && annexb[end - 1] == 0 {
                end -= 1;
            }
            nals.push(&annexb[start..end]);
        }
        let mut sps = None;
        let mut pps = None;
        let mut units = Vec::new();
        for nal in nals {
            match nal[0] & 0x1f {
                7 => sps = sps.or(Some(nal)),
                8 => pps = pps.or(Some(nal)),
                kind @ (1 | 5) => {
                    let mut sample = Vec::with_capacity(nal.len() + 4);
                    sample.extend((nal.len() as u32).to_be_bytes());
                    sample.extend(nal);
                    units.push(AccessUnit {
                        idr: kind == 5,
                        sample,
                    });
                }
                _ => {}
            }
        }
        let sps = sps.expect("the clip has an SPS");
        let pps = pps.expect("the clip has a PPS");
        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        avcc.extend((sps.len() as u16).to_be_bytes());
        avcc.extend(sps);
        avcc.push(1);
        avcc.extend((pps.len() as u16).to_be_bytes());
        avcc.extend(pps);
        assert!(
            units.first().is_some_and(|u| u.idr),
            "the clip starts at an IDR"
        );
        Clip { units, avcc }
    }
}

// ---------------------------------------------------------------------------
// The stream's queue: one thread, wakes, timers and closures.

enum Job {
    Wake,
    Timer(Instant, TimerToken),
    Run(Box<dyn FnOnce() + Send>),
}

#[derive(PartialEq, Eq)]
struct Due(Instant, TimerToken);
impl PartialOrd for Due {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Due {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.cmp(&self.0)
    }
}

struct ThreadQueue {
    tx: Mutex<Option<mpsc::Sender<Job>>>,
}

impl ThreadQueue {
    fn new(port: Arc<StreamPort>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("video-host-stream".into())
            .spawn(move || {
                let mut timers = BinaryHeap::<Due>::new();
                loop {
                    let job = match timers.peek() {
                        Some(Due(at, _)) => {
                            match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                                Ok(job) => Some(job),
                                Err(mpsc::RecvTimeoutError::Timeout) => None,
                                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                        None => match rx.recv() {
                            Ok(job) => Some(job),
                            Err(_) => return,
                        },
                    };
                    match job {
                        Some(Job::Wake) => port.run(),
                        Some(Job::Timer(at, token)) => timers.push(Due(at, token)),
                        Some(Job::Run(f)) => f(),
                        None => {}
                    }
                    while timers
                        .peek()
                        .is_some_and(|Due(at, _)| *at <= Instant::now())
                    {
                        if let Some(Due(_, token)) = timers.pop() {
                            port.run_timer(token);
                        }
                    }
                }
            })
            .expect("stream queue thread");
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
        })
    }

    fn send(&self, job: Job) {
        if let Some(tx) = lock(&self.tx).as_ref() {
            let _ = tx.send(job);
        }
    }

    fn run_on(&self, f: impl FnOnce() + Send + 'static) {
        self.send(Job::Run(Box::new(f)));
    }

    /// End the thread once the stream is closed.
    fn quit(&self) {
        lock(&self.tx).take();
    }
}

impl SerialQueue for ThreadQueue {
    fn wake(&self) {
        self.send(Job::Wake);
    }
    fn wake_after(&self, delay_ns: u64, timer: TimerToken) {
        self.send(Job::Timer(
            Instant::now() + Duration::from_nanos(delay_ns),
            timer,
        ));
    }
}

// ---------------------------------------------------------------------------
// Capture: a timer at the requested rate. Surfaces are tokens; the synthetic
// encoder ignores them.

const CAPTURE_ID: CaptureId = 1;

struct TimerCapture {
    queue: Arc<ThreadQueue>,
    fps: AtomicU32,
    events: Mutex<Option<Arc<dyn CaptureEvents>>>,
    running: Mutex<Option<Arc<AtomicBool>>>,
    next_surface: Arc<AtomicU64>,
}

impl TimerCapture {
    fn stop_timer(&self) {
        if let Some(running) = lock(&self.running).take() {
            running.store(false, Ordering::Release);
        }
    }
}

impl CaptureBackend for TimerCapture {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            target_kinds: vec![CaptureTargetKind::Display],
            window_fit: false,
            live_region_move: false,
            cursor_modes: vec![CursorMode::Hidden],
            surface_kinds: vec![SurfaceKind::CpuMapped],
            pool_depth: 5,
            virtual_displays: false,
            region_in_surface: false,
        }
    }
    fn enumerate(&self) -> Result<SourceCatalog, MediaError> {
        Ok(SourceCatalog {
            displays: vec![],
            windows: vec![],
        })
    }
    fn observe_layout(&self, _display: u32) -> Option<DisplayLayout> {
        None
    }
    fn open(
        &self,
        _spec: CaptureSpec,
        _queue: Arc<dyn SerialQueue>,
        events: Arc<dyn CaptureEvents>,
    ) -> Result<CaptureId, MediaError> {
        *lock(&self.events) = Some(events);
        Ok(CAPTURE_ID)
    }
    fn start(&self, id: CaptureId, op: OpToken) {
        self.stop_timer();
        let Some(events) = lock(&self.events).clone() else {
            return;
        };
        let running = Arc::new(AtomicBool::new(true));
        *lock(&self.running) = Some(running.clone());
        let interval = Duration::from_secs(1) / self.fps.load(Ordering::Relaxed).max(1);
        let queue = self.queue.clone();
        let next_surface = self.next_surface.clone();
        let frame_events = events.clone();
        std::thread::Builder::new()
            .name("video-host-capture".into())
            .spawn(move || {
                let mut due = Instant::now();
                while running.load(Ordering::Acquire) {
                    let events = frame_events.clone();
                    let surface = next_surface.fetch_add(1, Ordering::Relaxed);
                    // Delivered on the stream's queue, as a capture backend does.
                    queue.run_on(move || {
                        events.frame(
                            id,
                            None,
                            SurfaceInfo {
                                id: surface,
                                kind: SurfaceKind::CpuMapped,
                                format: PixelFormat::Nv12,
                                width: WIDTH,
                                height: HEIGHT,
                                color: ColorTags::BT709,
                                capture_ns: host_ns(),
                                source_present_ns: None,
                                dirty: None,
                                rotation: Rotation::None,
                                fence: GpuFence::None,
                            },
                            FrameStatus::Complete,
                        );
                    });
                    due += interval;
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                }
            })
            .expect("capture thread");
        events.op_done(op, None);
    }
    fn update(&self, _id: CaptureId, update: CaptureUpdate, op: OpToken) {
        if let Some(interval) = update.min_interval_ns.filter(|ns| *ns > 0) {
            let fps = (1_000_000_000 / interval).clamp(1, 240) as u32;
            self.fps.store(fps, Ordering::Relaxed);
        }
        if let Some(events) = lock(&self.events).clone() {
            events.op_done(op, None);
        }
    }
    fn stop(&self, _id: CaptureId, op: OpToken) {
        self.stop_timer();
        if let Some(events) = lock(&self.events).clone() {
            events.op_done(op, None);
        }
    }
    fn release(&self, _surface: SurfaceId) {}
}

// ---------------------------------------------------------------------------
// The synthetic encoder: each submit outputs the clip's next access unit.

struct ClipFactory {
    clip: Arc<Clip>,
}

impl EncoderFactory for ClipFactory {
    fn probe(&self, _adapter: Option<u64>) -> Vec<EncoderCaps> {
        vec![]
    }
    fn create(
        &self,
        spec: EncoderSpec,
        events: Arc<dyn EncoderEvents>,
    ) -> Result<Arc<dyn Encoder>, MediaError> {
        if spec.codec != Codec::H264 {
            return Err(MediaError::Unsupported);
        }
        Ok(Arc::new(ClipEncoder {
            clip: self.clip.clone(),
            sink: spec.sink.clone(),
            events,
            cursor: Mutex::new(0),
            next_token: AtomicU64::new(1),
        }))
    }
}

struct ClipEncoder {
    clip: Arc<Clip>,
    sink: Arc<VideoSinkHandle>,
    events: Arc<dyn EncoderEvents>,
    cursor: Mutex<usize>,
    next_token: AtomicU64,
}

impl Encoder for ClipEncoder {
    fn info(&self) -> EncoderInfo {
        EncoderInfo {
            codec: Codec::H264,
            name: "synthetic clip".into(),
            hardware: false,
            low_latency: true,
        }
    }
    fn submit(
        &self,
        _frame: SurfaceId,
        opts: SubmitOpts,
        permit: Arc<VideoWorkPermit>,
    ) -> SubmitResult {
        let input_ns = host_ns();
        let index = {
            let mut cursor = lock(&self.cursor);
            let units = &self.clip.units;
            if opts.force_keyframe && !units[*cursor].idr {
                // The next IDR (the clip loops at frame 0, an IDR).
                *cursor = (*cursor..units.len()).find(|&i| units[i].idr).unwrap_or(0);
            }
            let index = *cursor;
            *cursor = (index + 1) % units.len();
            index
        };
        let unit = &self.clip.units[index];
        let payload_token = self.next_token.fetch_add(2, Ordering::Relaxed);
        if !self.sink.submit_encoded(payload_token, unit.sample.clone()) {
            self.events.dropped(opts.submit_seq, DropReason::NoOutput);
            return SubmitResult::Submitted;
        }
        let config_token = unit.idr.then(|| {
            let token = payload_token + 1;
            self.sink.submit_encoded(token, self.clip.avcc.clone());
            token
        });
        // Output at once, as a fast hardware encoder would; it only posts.
        self.events.encoded(EncodedFrame {
            submit_seq: opts.submit_seq,
            pts_ns: opts.pts_ns,
            keyframe: unit.idr,
            config_token,
            payload_token,
            form: BitstreamForm::Avcc4,
            discardable: false,
            temporal_id: None,
            ltr: None,
            timings: EncodeTimings {
                input_ns: Some(input_ns),
                output_ns: Some(host_ns()),
            },
        });
        drop(permit);
        SubmitResult::Submitted
    }
    fn set_rate(&self, _update: RateUpdate) -> Result<(), MediaError> {
        Ok(())
    }
    fn invalidate(&self) -> InvalidateOutcome {
        InvalidateOutcome::Completed
    }
}

// ---------------------------------------------------------------------------
// Observer.

struct Printer;

impl ObserverQueue for Printer {
    fn post(&self, job: ObserverJob) {
        match job {
            ObserverJob::Stats { snapshot } => println!(
                "stats: captured {} encoded {} packets sent {} recoveries {} client feedback {} client lost {} sink shed {}",
                snapshot.frames_received,
                snapshot.encoded,
                snapshot.packets_sent,
                snapshot.recoveries,
                snapshot.client_feedback,
                snapshot.client_lost,
                snapshot.sender_shed
            ),
            other => println!("stream: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// One client's stream.

struct Stream {
    controller: Arc<StreamController>,
    queue: Arc<ThreadQueue>,
}

fn start_stream(channel: Arc<SecureChannel>, clip: Arc<Clip>, fps: u32) -> Stream {
    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let capture = Arc::new(TimerCapture {
        queue: queue.clone(),
        fps: AtomicU32::new(fps),
        events: Mutex::new(None),
        running: Mutex::new(None),
        next_surface: Arc::new(AtomicU64::new(1)),
    });
    let plan = StreamPlan {
        mode: StreamMode::Control,
        codec: Codec::H264,
        encode_width: WIDTH,
        encode_height: HEIGHT,
        fps,
        bitrate_bps: 800_000,
        convert: ConvertPath::Identity,
        encoder_input: PixelFormat::Nv12,
        encoder: "synthetic clip".into(),
        color_out: ColorTags::BT709,
        color_mapping: ColorMapping {
            allow_p3: false,
            srgb_transfer_as_bt709: true,
        },
        extensions: if channel.frames_v4() {
            vec![FrameExtension::Timing, FrameExtension::CaptureUs]
        } else {
            vec![]
        },
        permit_target: 1,
        recovery: vec![RecoveryStep::Idr],
        on_source_gone: OnSourceGone::Fail,
        medium_cap_bps: None,
        adapter_luid: None,
        refresh_interval_ns: None,
        pacing: PacingPolicy::default(),
    };
    let spec = CaptureSpec {
        target: CaptureTarget::Display { display_id: 1 },
        mode: StreamMode::Control,
        pixel_width: WIDTH,
        pixel_height: HEIGHT,
        min_interval_ns: 1_000_000_000 / u64::from(fps.max(1)),
        format: PixelFormat::Nv12,
        color: ColorRequest::Source,
        cursor: CursorMode::Hidden,
        queue_depth: 5,
        adapter_luid: None,
    };
    let controller = StreamController::new(StreamParts {
        stream_id: STREAM_ID,
        spec,
        plan,
        port,
        queue: queue.clone(),
        capture,
        converter: None,
        encoders: Arc::new(ClipFactory { clip }),
        output: VideoOutput::channel(channel, SinkPolicy::default(), None),
        observer: Arc::new(Printer),
        timings: None,
    })
    .expect("a valid stream");
    controller.start();
    Stream { controller, queue }
}

/// The client's messages: `videoFeedback` on lane 0 and the demo's keyframe
/// request (envelope kind `0x01`, `{"type":"keyframe"}`).
async fn serve_client(channel: Arc<SecureChannel>, controller: Arc<StreamController>) {
    while let Ok(message) = channel.receive_timed().await {
        let Ok(Some((frame, _))) = Frame::read(&message.payload) else {
            continue;
        };
        match frame.kind {
            KIND_VIDEO_FEEDBACK => {
                let Ok(feedback) = VideoFeedback::decode(frame.payload) else {
                    continue;
                };
                let (loss_base_sequence, loss_bits) = feedback
                    .loss
                    .map_or((0, Vec::new()), |loss| (loss.base_sequence, loss.bits));
                controller.note_client_feedback(ClientFeedback {
                    generation: feedback.generation,
                    loss_base_sequence,
                    loss_bits,
                    frames: feedback
                        .frames
                        .iter()
                        .map(|f| ClientFrameTimes {
                            sequence: f.sequence,
                            receive_us: f.receive_us,
                            decode_us: f.decode_us,
                            present_us: f.present_us,
                        })
                        .collect(),
                });
                if let Some(token) = feedback.ltr_ack {
                    controller.note_ltr_ack(feedback.generation, token);
                }
            }
            0x01 => {
                let text = String::from_utf8_lossy(frame.payload);
                if text.replace(' ', "").contains("\"type\":\"keyframe\"") {
                    println!("client: keyframe request");
                    controller.request_keyframe();
                }
            }
            _ => {}
        }
    }
    println!("client: channel closed");
}

// ---------------------------------------------------------------------------

struct Args {
    listen: String,
    pair_port: Option<u16>,
    fps: u32,
}

fn args() -> Args {
    let mut listen = None;
    let mut pair_port = None;
    let mut fps = 60;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--listen" => listen = Some(value()),
            "--pair-port" => pair_port = Some(value().parse().unwrap_or_else(|_| usage())),
            "--fps" => fps = value().parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    Args {
        listen: listen.unwrap_or_else(|| usage()),
        pair_port,
        fps: fps.clamp(1, 240),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: video_host --listen <lan-ip>:<port> [--pair-port <port>] [--fps <1-240>]\n\
         Listen on the LAN address a client can reach (not 0.0.0.0): it is printed in the connect line."
    );
    std::process::exit(2)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args = args();
    let listen: std::net::SocketAddr = args.listen.parse().unwrap_or_else(|_| usage());
    let pair_port = args.pair_port.unwrap_or(listen.port().wrapping_add(1));
    let pair_address = std::net::SocketAddr::new(listen.ip(), pair_port);
    let clip = Arc::new(Clip::parse(CLIP));
    println!(
        "clip: {} frames, {} IDR, {}x{} H.264, looped at {} fps",
        clip.units.len(),
        clip.units.iter().filter(|u| u.idr).count(),
        WIDTH,
        HEIGHT,
        args.fps
    );

    let identity = Arc::new(PeerIdentity::generate().expect("an identity"));
    let listener = listen_secure(listen.to_string(), identity.clone(), vec![], options())
        .await
        .expect("listening on the stream port");
    let trusted = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));

    // Pairing: one window at a time, reopened (with a new PIN) when it is used
    // or expires.
    {
        let identity = identity.clone();
        let listener = listener.clone();
        let trusted = trusted.clone();
        let host = listen.ip();
        let port = listen.port();
        tokio::spawn(async move {
            loop {
                let service = match listen_pairing(pair_address.to_string(), identity.clone()).await
                {
                    Ok(service) => service,
                    Err(error) => {
                        eprintln!("pairing: cannot listen on {pair_address}: {error}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };
                if let Ok(pin) = service.display_code() {
                    let host = match host {
                        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                        v4 => v4.to_string(),
                    };
                    println!(
                        "connect: streamcore-video://{host}:{port}?pair={pair_port}&pin={pin}&fp={}",
                        identity.fingerprint()
                    );
                }
                let verified = tokio::time::timeout(PAIRING_WINDOW, service.next_verified()).await;
                if let Ok(Ok(pending)) = verified {
                    let certificate = pending.peer_certificate();
                    let pins = {
                        let mut trusted = lock(&trusted);
                        trusted.push(certificate);
                        trusted.clone()
                    };
                    // Trust before confirming (the host persists first).
                    if listener.replace_trusted_peers(pins).is_ok()
                        && pending.confirm_persisted().await.is_ok()
                    {
                        println!("pairing: a client paired");
                    } else {
                        let _ = pending.reject_persistence();
                    }
                }
                let _ = service.close_and_wait().await;
            }
        });
    }

    let mut current: Option<Stream> = None;
    let mut stats = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let channel = match accepted {
                    Ok(channel) => channel,
                    Err(error) => {
                        eprintln!("accept: {error}");
                        continue;
                    }
                };
                println!(
                    "client: connected over {} from {}, frames.v4 {}",
                    match channel.transport() {
                        ChannelTransport::Quic => "QUIC",
                        ChannelTransport::TlsTcp => "TLS/TCP",
                    },
                    channel.remote_address(),
                    channel.frames_v4()
                );
                if let Some(old) = current.take() {
                    // The queue thread ends once the old stream's close completed.
                    tokio::spawn(async move {
                        old.controller.close().await;
                        old.queue.quit();
                    });
                }
                let stream = start_stream(channel.clone(), clip.clone(), args.fps);
                tokio::spawn(serve_client(channel, stream.controller.clone()));
                current = Some(stream);
            }
            _ = stats.tick() => {
                if let Some(stream) = &current {
                    stream.controller.request_stats();
                }
            }
        }
    }
}
