//! A headless client for `video_host`: pairs with the PIN of a connect line,
//! connects, runs a `ClientStream` over a decoder that only counts (it decodes
//! nothing), and prints the stream's stats every second. It checks a host
//! (and the network between) without a device.
//!
//! ```text
//! cargo run --release -p streamcore-video --example video_client -- 'streamcore-video://<host>:7700?pair=7701&pin=123456&fp=<sha-256>' [--seconds 10]
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use streamcore_media::binary::Frame;
use streamcore_media::media::FRAMES_V4_CAPABILITY;
use streamcore_transport::OperationCancellation;
use streamcore_transport::channel::{
    ChannelPath, ChannelTransport, SecureChannel, SessionOptions, connect_secure,
};
use streamcore_transport::identity::PeerIdentity;
use streamcore_transport::pairing_transport::pair_with_host;
use streamcore_video::*;

const DEMO_CAPABILITY: &str = "streamcore.video-demo.v1";
const STREAM_ID: u32 = 1;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `streamcore-video://<host>:<port>?pair=<port>&pin=<pin>&fp=<fingerprint>`.
struct ConnectLine {
    stream: String,
    pairing: String,
    pin: String,
    fingerprint: String,
}

fn parse(line: &str) -> Option<ConnectLine> {
    let rest = line.trim().strip_prefix("streamcore-video://")?;
    let (authority, query) = rest.split_once('?')?;
    let (host, _) = authority.rsplit_once(':')?;
    let field = |name: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_owned)
    };
    Some(ConnectLine {
        stream: authority.to_owned(),
        pairing: format!("{host}:{}", field("pair")?),
        pin: field("pin")?,
        fingerprint: field("fp")?,
    })
}

/// The stream's queue: one thread draining wakes and timers.
struct ThreadQueue {
    tx: Mutex<mpsc::Sender<Option<(Instant, TimerToken)>>>,
}

impl ThreadQueue {
    fn new(port: Arc<StreamPort>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Option<(Instant, TimerToken)>>();
        std::thread::spawn(move || {
            let mut timers: Vec<(Instant, TimerToken)> = Vec::new();
            loop {
                let wait = timers
                    .iter()
                    .map(|(at, _)| at.saturating_duration_since(Instant::now()))
                    .min()
                    .unwrap_or(Duration::from_secs(1));
                match rx.recv_timeout(wait) {
                    Ok(None) => port.run(),
                    Ok(Some(timer)) => timers.push(timer),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                let now = Instant::now();
                let (due, later): (Vec<_>, Vec<_>) =
                    timers.drain(..).partition(|(at, _)| *at <= now);
                timers = later;
                for (_, token) in due {
                    port.run_timer(token);
                }
            }
        });
        Arc::new(Self { tx: Mutex::new(tx) })
    }
}

impl SerialQueue for ThreadQueue {
    fn wake(&self) {
        let _ = lock(&self.tx).send(None);
    }
    fn wake_after(&self, delay_ns: u64, timer: TimerToken) {
        let at = Instant::now() + Duration::from_nanos(delay_ns);
        let _ = lock(&self.tx).send(Some((at, timer)));
    }
}

/// Accepts every sample and reports it decoded and presented at once.
struct CountingDecoder {
    events: Mutex<Option<Arc<dyn DecoderEvents>>>,
    bytes: AtomicU64,
    anchor: Instant,
}

impl StreamDecoder for CountingDecoder {
    fn configure(
        &self,
        _codec: Codec,
        config: Vec<u8>,
        color: ColorTags,
        _queue: Arc<dyn SerialQueue>,
        events: Arc<dyn DecoderEvents>,
    ) -> Result<DecoderInfo, MediaError> {
        println!(
            "decoder: configure, {} bytes of avcC, {color:?}",
            config.len()
        );
        *lock(&self.events) = Some(events);
        Ok(DecoderInfo {
            low_latency_confirmed: true,
            hardware: false,
            max_queued: 0,
            notes: vec!["counting only".into()],
        })
    }
    fn decode(&self, sample: Vec<u8>, meta: SampleMeta) -> DecodeResult {
        self.bytes.fetch_add(sample.len() as u64, Ordering::Relaxed);
        if let Some(events) = lock(&self.events).clone() {
            let now = self.now_ns();
            events.decoded(meta.seq, now);
            events.presented(meta.seq, now, true);
        }
        DecodeResult::Accepted
    }
    fn flush(&self) -> InvalidateOutcome {
        InvalidateOutcome::Completed
    }
    fn set_mode(&self, _mode: PresentMode) {}
    fn now_ns(&self) -> u64 {
        u64::try_from(self.anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// The consumer's jobs: keyframe requests go to the host as the demo's
/// control message.
struct Jobs {
    channel: Arc<SecureChannel>,
}

impl ClientEvents for Jobs {
    fn post(&self, job: ClientJob) {
        match job {
            ClientJob::RequestKeyframe { reason } => {
                println!("client: keyframe request ({reason:?})");
                let message = Frame {
                    kind: 0x01,
                    payload: br#"{"type":"keyframe"}"#,
                }
                .encode()
                .expect("a small frame");
                let _ = self.channel.enqueue_application_frame(message);
            }
            ClientJob::Stats { .. } | ClientJob::Feedback { .. } => {}
            other => println!("client: {other:?}"),
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let usage = || -> ! {
        eprintln!("usage: video_client '<connect line>' [--seconds <n>]");
        std::process::exit(2)
    };
    let line = parse(&args.next().unwrap_or_else(|| usage())).unwrap_or_else(|| usage());
    let mut seconds = 10u64;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                seconds = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            _ => usage(),
        }
    }

    let identity = Arc::new(PeerIdentity::generate().expect("an identity"));
    let host = pair_with_host(
        vec![line.pairing.clone()],
        identity.clone(),
        line.pin.clone(),
        Some(line.fingerprint.clone()),
        OperationCancellation::new(),
    )
    .await
    .unwrap_or_else(|error| {
        eprintln!("pairing failed: {error}");
        std::process::exit(1)
    });
    println!("paired");
    let options = SessionOptions {
        capabilities: vec![DEMO_CAPABILITY.into(), FRAMES_V4_CAPABILITY.into()],
        required: vec![DEMO_CAPABILITY.into()],
    };
    let channel = connect_secure(
        identity,
        host,
        vec![ChannelPath {
            address: line.stream.clone(),
            transport: ChannelTransport::TlsTcp,
            delay_ms: 0,
        }],
        options,
        OperationCancellation::new(),
    )
    .await
    .unwrap_or_else(|error| {
        eprintln!("connect failed: {error}");
        std::process::exit(1)
    });
    println!("connected, frames.v4 {}", channel.frames_v4());

    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let decoder = Arc::new(CountingDecoder {
        events: Mutex::new(None),
        bytes: AtomicU64::new(0),
        anchor: Instant::now(),
    });
    let stream = ClientStream::new(ClientParts {
        stream_id: STREAM_ID,
        codec: Codec::H264,
        color: ColorTags::BT709,
        mode: PresentMode::Responsive,
        port,
        queue,
        decoder: decoder.clone(),
        events: Arc::new(Jobs {
            channel: channel.clone(),
        }),
        channel: Some(channel.clone()),
        timings: None,
    })
    .expect("a client stream");
    {
        let stream = stream.clone();
        let channel = channel.clone();
        tokio::spawn(async move { while stream.next_message(channel.clone()).await.is_ok() {} });
    }
    let mut last = 0;
    for _ in 0..seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let stats = stream.stats();
        println!(
            "decoded {} (+{}/s), {} kB, queue bound {}, flushed {}, keyframe requests {}, feedback sent {}",
            stats.decoded,
            stats.decoded - last,
            decoder.bytes.load(Ordering::Relaxed) / 1000,
            stats.queue_bound,
            stats.flushed,
            stats.keyframe_requests,
            stats.feedback_sent
        );
        last = stats.decoded;
    }
    stream.close().await;
    channel.close();
}
