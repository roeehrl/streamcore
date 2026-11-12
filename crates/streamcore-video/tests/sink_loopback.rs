//! `VideoSink` on a real `SecureChannel` over loopback TLS/TCP, alongside a real
//! bulk transfer (6.4 rule 5): the bulk token bucket is live while the sink
//! carries video, bulk runs at the bucket's rate, and video keeps arriving with
//! a bounded delay while bulk is queued.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use streamcore_media::binary::Frame;
use streamcore_media::media::{ExtendedVideoPacket, FRAMES_V4_CAPABILITY, FrameVersion};
use streamcore_transport::channel::{
    ChannelPath, ChannelTransport, SecureChannel, SecureListener, SessionOptions, connect_secure,
    listen_secure,
};
use streamcore_transport::clock::monotonic_now_us;
use streamcore_transport::identity::PeerIdentity;
use streamcore_transport::tuning::{TransportTuning, set_transport_tuning};
use streamcore_video::*;
use support::*;

const BULK_MESSAGE: usize = 256 * 1024;
const BULK_MESSAGES: usize = 48;
const CAPACITY_BPS: u64 = 100_000_000;

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec!["inventory.v1".into(), FRAMES_V4_CAPABILITY.into()],
        required: vec!["inventory.v1".into()],
    }
}

async fn channels() -> (Arc<SecureListener>, Arc<SecureChannel>, Arc<SecureChannel>) {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    // Loopback, built from parts.
    let address = format!("{}.{}.{}.{}:0", 127, 0, 0, 1);
    let listener = listen_secure(address, host.clone(), vec![client.certificate()], options())
        .await
        .unwrap();
    let accept = listener.clone();
    let server = tokio::spawn(async move { accept.accept().await.unwrap() });
    let connected = connect_secure(
        client,
        host.certificate(),
        vec![ChannelPath {
            address: listener.address(),
            transport: ChannelTransport::TlsTcp,
            delay_ms: 0,
        }],
        options(),
        streamcore_transport::OperationCancellation::new(),
    )
    .await
    .unwrap();
    (listener, server.await.unwrap(), connected)
}

#[derive(Default)]
struct Received {
    /// (sequence, arrival µs) of each video payload.
    video: Vec<(u64, u64)>,
    bulk: usize,
    bulk_done_us: Option<u64>,
    frame_timing: usize,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_and_a_bulk_transfer_share_the_link_through_the_bucket() {
    // The bucket must be on before the channel exists (the host's startup).
    set_transport_tuning(TransportTuning {
        bulk_token_bucket: true,
        ..TransportTuning::default()
    });
    let (_listener, host, client) = channels().await;
    assert!(host.bulk_bucket_enabled() && host.frames_v4());

    // The client consumes everything, as a real client does.
    let received = Arc::new(std::sync::Mutex::new(Received::default()));
    let reader = {
        let received = received.clone();
        let client = client.clone();
        tokio::spawn(async move {
            while let Ok(message) = client.receive_timed().await {
                let (frame, _) = Frame::read(&message.payload).unwrap().unwrap();
                let mut r = received.lock().unwrap();
                match frame.kind {
                    0x10 => {
                        let packet = ExtendedVideoPacket::decode(frame.payload, FrameVersion::V4)
                            .unwrap_or_else(|e| {
                                panic!("{e:?} {:?}", &frame.payload[..frame.payload.len().min(60)])
                            });
                        if packet.metadata.flags != 2 {
                            r.video
                                .push((packet.metadata.sequence, message.complete_us));
                        }
                    }
                    0x14 => r.frame_timing += 1,
                    0x40 => {
                        r.bulk += 1;
                        if r.bulk == BULK_MESSAGES {
                            r.bulk_done_us = Some(message.complete_us);
                        }
                    }
                    _ => {}
                }
            }
        })
    };

    let s = Manual::with(|_, parts| {
        parts.plan.extensions = vec![FrameExtension::Timing, FrameExtension::CaptureUs];
        parts.output = VideoOutput::channel(
            host.clone(),
            SinkPolicy {
                frame_timing_every: 4,
                ..SinkPolicy::default()
            },
            Some(CAPACITY_BPS),
        );
    });
    s.start_running();

    // Video first, so the bucket has its rate inputs and video counts as active.
    let mut sent: Vec<(u64, u64)> = Vec::new();
    let tick = |s: &Manual, n: u64, sent: &mut Vec<(u64, u64)>| {
        s.frame(n * 1_000_000);
        let encoder = s.encoder();
        for seq in encoder.pending_seqs() {
            sent.push((seq, monotonic_now_us()));
            encoder.complete(seq);
        }
        s.pump();
    };
    for n in 1..=5 {
        tick(&s, n, &mut sent);
        tokio::time::sleep(Duration::from_millis(16)).await;
        s.advance_ms(16);
    }
    let limit = host
        .bulk_limit_bytes_per_second()
        .expect("the bucket has rate inputs");
    assert!(limit < CAPACITY_BPS / 8, "bulk below the link capacity");

    // A real bulk transfer: queued at once, as a file pump would.
    let bulk_start = monotonic_now_us();
    let payload = vec![0x5a; BULK_MESSAGE];
    for _ in 0..BULK_MESSAGES {
        let frame = Frame {
            kind: 0x40,
            payload: &payload,
        }
        .encode()
        .unwrap();
        host.enqueue_application_frame(frame).unwrap();
    }

    // Video at 60 fps while bulk drains.
    let done = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut n = 6;
    while !done.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "bulk never finished");
        tick(&s, n, &mut sent);
        n += 1;
        tokio::time::sleep(Duration::from_millis(16)).await;
        s.advance_ms(16);
        if received.lock().unwrap().bulk_done_us.is_some() {
            done.store(true, Ordering::SeqCst);
        }
    }
    // Let the last frames arrive.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(16)).await;
        s.pump();
    }

    let r = std::mem::take(&mut *received.lock().unwrap());
    let bulk_us = r.bulk_done_us.unwrap() - bulk_start;
    let bulk_bytes = (BULK_MESSAGE * BULK_MESSAGES) as u64;
    // The bucket held bulk near its rate: unlimited loopback takes milliseconds.
    let at_limit_us = bulk_bytes * 1_000_000 / limit;
    assert!(
        bulk_us * 10 >= at_limit_us * 7,
        "bulk took {bulk_us} µs, the bucket allows {at_limit_us} µs"
    );
    // Video kept flowing during the transfer, each frame within 100 ms.
    let during = r
        .video
        .iter()
        .filter(|(_, at)| *at > bulk_start && *at < bulk_start + bulk_us)
        .count();
    assert!(during >= 10, "only {during} video frames during bulk");
    let mut worst = 0;
    for (seq, at) in &r.video {
        if let Some((_, sent_us)) = sent.iter().find(|(s, _)| s == seq) {
            worst = worst.max(at.saturating_sub(*sent_us));
        }
    }
    eprintln!(
        "bulk {bulk_bytes} B in {bulk_us} µs (bucket {limit} B/s, {at_limit_us} µs at the limit); \
         {during} video frames during bulk, worst video delay {worst} µs"
    );
    assert!(worst < 100_000, "a video frame took {worst} µs");
    assert!(r.frame_timing > 0, "frameTiming under frames.v4");
    drop(r);

    let stats = s.controller.stats();
    assert!(!stats.bulk_bucket_inactive && stats.frames_v4);
    assert_eq!(stats.bulk_capacity_bps, CAPACITY_BPS);
    assert_eq!(stats.packets_refused, 0);
    s.close();
    s.assert_clean();
    host.close();
    client.close();
    let _ = reader.await;
}
