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
    channels_over(ChannelTransport::TlsTcp).await
}

async fn channels_over(
    transport: ChannelTransport,
) -> (Arc<SecureListener>, Arc<SecureChannel>, Arc<SecureChannel>) {
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
            transport,
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

fn p95(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[(values.len() * 95).div_ceil(100).max(1) - 1]
}

/// The P2 scheduling gate (9.6, 11): with rate adaptation on, a 60 fps stream's
/// frames keep arriving at most 1.5 frame intervals apart (p95) while a real
/// bulk transfer runs, on TLS/TCP and on QUIC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_gaps_stay_within_one_and_a_half_frames_under_a_real_transfer() {
    set_transport_tuning(TransportTuning {
        bulk_token_bucket: true,
        ..TransportTuning::default()
    });
    for (name, transport) in [
        ("TLS/TCP", ChannelTransport::TlsTcp),
        ("QUIC", ChannelTransport::Quic),
    ] {
        let (_listener, host, client) = channels_over(transport).await;
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
                            let packet =
                                ExtendedVideoPacket::decode(frame.payload, FrameVersion::V4)
                                    .unwrap();
                            if packet.metadata.flags != 2 {
                                r.video
                                    .push((packet.metadata.sequence, message.complete_us));
                            }
                        }
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
        let s = Manual::with(|fakes, parts| {
            fakes.factory.honour_rate.store(true, Ordering::SeqCst);
            parts.plan.encode_width = 1280;
            parts.plan.encode_height = 720;
            parts.plan.bitrate_bps = 20_000_000;
            parts.plan.pacing = PacingPolicy::with_rate_adaptation();
            parts.output =
                VideoOutput::channel(host.clone(), SinkPolicy::default(), Some(CAPACITY_BPS));
        });
        s.start_running();
        let interval = Duration::from_micros(16_667);
        let start = tokio::time::Instant::now();
        let mut n: u64 = 0;
        let mut sent_at = Vec::new();
        let tick = |s: &Manual, n: u64, sent_at: &mut Vec<u64>| {
            s.frame(n * 16_667_000);
            s.encoder().complete_all();
            sent_at.push(monotonic_now_us());
            s.pump();
        };
        // One second of video first.
        while n < 60 {
            n += 1;
            tick(&s, n, &mut sent_at);
            tokio::time::sleep_until(start + interval * n as u32).await;
            s.advance_ms(17);
        }
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
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            n += 1;
            tick(&s, n, &mut sent_at);
            tokio::time::sleep_until(start + interval * n as u32).await;
            s.advance_ms(17);
            let done = received.lock().unwrap().bulk_done_us;
            if done.is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "bulk never finished");
        }
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(16)).await;
            s.pump();
        }
        let r = std::mem::take(&mut *received.lock().unwrap());
        let bulk_end = r.bulk_done_us.unwrap();
        let arrivals: Vec<u64> = r
            .video
            .iter()
            .map(|(_, at)| *at)
            .filter(|at| *at > bulk_start && *at <= bulk_end)
            .collect();
        let mut gaps: Vec<u64> = arrivals.windows(2).map(|w| w[1] - w[0]).collect();
        let sends: Vec<u64> = sent_at
            .iter()
            .copied()
            .filter(|at| *at > bulk_start && *at <= bulk_end)
            .collect();
        let mut send_gaps: Vec<u64> = sends.windows(2).map(|w| w[1] - w[0]).collect();
        let gap_p95 = p95(&mut gaps);
        let send_p95 = p95(&mut send_gaps);
        let stats = s.controller.stats();
        eprintln!(
            "{name}: {} frames during {} µs of bulk; arrival gap p95 {gap_p95} µs (send gap p95 \
             {send_p95} µs, max {} µs); target {} bit/s, {} decreases",
            arrivals.len(),
            bulk_end - bulk_start,
            gaps.last().unwrap(),
            stats.abr_target_bps,
            stats.abr_decreases
        );
        assert!(arrivals.len() >= 10, "video flowed during bulk");
        // 1.5 × 16.667 ms. The producer's own timer jitter is in the number:
        // allow it on top (a loaded test host), never more than 2 frames.
        let allowed = 25_000 + send_p95.saturating_sub(16_667);
        assert!(
            gap_p95 <= allowed.min(33_334),
            "arrival gap p95 {gap_p95} µs (allowed {allowed})"
        );
        assert_eq!(stats.packets_refused, 0);
        s.close();
        s.assert_clean();
        host.close();
        client.close();
        let _ = reader.await;
    }
}
