//! `ClientStream` on a real `SecureChannel` over loopback TLS/TCP: the host
//! sends a stream's configuration and frames on lane 1 next to a control
//! message; the client's receive loop (`next_message`) hands the frames to
//! the decoder and returns the control message; the client's `videoFeedback`
//! arrives at the host on lane 0.

mod client_support;
mod support;

use client_support::*;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use streamcore_media::binary::Frame;
use streamcore_media::media::{
    ExtendedVideoPacket, FRAMES_V4_CAPABILITY, FrameVersion, VideoExtensions, VideoMetadata,
};
use streamcore_media::video_control::{KIND_VIDEO_FEEDBACK, VideoFeedback};
use streamcore_transport::channel::{
    ChannelPath, ChannelTransport, SecureChannel, SecureListener, SessionOptions, connect_secure,
    listen_secure,
};
use streamcore_transport::identity::PeerIdentity;
use streamcore_video::*;
use support::{ManualClock, ThreadQueue, lock};

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

fn video(sequence: u64, flags: u8, data: &[u8]) -> Vec<u8> {
    let packet = ExtendedVideoPacket {
        metadata: VideoMetadata {
            stream_id: STREAM,
            generation: 9,
            sequence,
            flags,
            pts_micros: if flags == 2 { 0 } else { sequence * 16_667 },
        },
        extensions: if flags == 2 {
            VideoExtensions::default()
        } else {
            VideoExtensions {
                capture_us: Some(sequence),
                ..VideoExtensions::default()
            }
        },
        data,
    }
    .encode(FrameVersion::V4)
    .unwrap();
    Frame {
        kind: 0x10,
        payload: &packet,
    }
    .encode()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_reach_the_decoder_and_feedback_reaches_the_host() {
    let (_listener, host, client) = channels().await;
    assert!(client.frames_v4());

    let port = StreamPort::new();
    let queue = ThreadQueue::new(port.clone());
    let closed = Arc::new(AtomicBool::new(false));
    let decoder = FakeDecoder::new(Arc::new(ManualClock::default()), closed.clone());
    *lock(&decoder.inline) = Inline::DecodeAndPresent;
    let jobs = Arc::new(Jobs {
        jobs: Mutex::default(),
        closed,
    });
    let stream = ClientStream::new(ClientParts {
        stream_id: STREAM,
        codec: Codec::H264,
        color: ColorTags::BT709,
        mode: PresentMode::Responsive,
        port,
        queue: queue.clone(),
        decoder: decoder.clone(),
        events: jobs.clone(),
        channel: Some(client.clone()),
        timings: None,
    })
    .unwrap();

    // The client's receive loop: video stays in Rust, the rest comes back
    // (here, kept in a list).
    let others = Arc::new(Mutex::new(Vec::new()));
    let receive = {
        let stream = stream.clone();
        let client = client.clone();
        let others = others.clone();
        tokio::spawn(async move {
            while let Ok(message) = stream.next_message(client.clone()).await {
                lock(&others).push(message.payload);
            }
        })
    };

    host.enqueue_application_frame(video(0, 2, b"avcC-bytes"))
        .unwrap();
    for sequence in 1..=10 {
        host.enqueue_application_frame(video(
            sequence,
            u8::from(sequence == 1),
            &[0, 0, 0, 2, 0x41, sequence as u8],
        ))
        .unwrap();
    }
    let control = Frame {
        kind: 0x01,
        payload: b"{\"type\":\"ping\"}",
    }
    .encode()
    .unwrap();
    host.enqueue_application_frame(control.clone()).unwrap();

    // Feedback arrives on the host's side.
    let mut reports: Vec<VideoFeedback> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while reports.iter().map(|r| r.frames.len()).sum::<usize>() < 10 {
        let Ok(message) = tokio::time::timeout_at(deadline, host.receive_timed()).await else {
            panic!(
                "no feedback: {:?} {:?} {:?}",
                stream.stats(),
                reports,
                lock(&jobs.jobs)
            );
        };
        let message = message.unwrap();
        let (frame, _) = Frame::read(&message.payload).unwrap().unwrap();
        assert_eq!(frame.kind, KIND_VIDEO_FEEDBACK);
        reports.push(VideoFeedback::decode(frame.payload).unwrap());
    }
    queue.flush();
    // Lane 0 may overtake lane 1: the control message came back, once.
    assert_eq!(*lock(&others), [control]);
    assert_eq!(decoder.decode_seqs(), (1..=10).collect::<Vec<_>>());
    assert_eq!(decoder.configures(), 1);
    let last = reports.last().unwrap();
    assert_eq!(last.stream_id, STREAM);
    assert_eq!(last.generation, 9);
    assert_eq!(last.highest_complete_sequence, 10);
    let mut sequences: Vec<_> = reports
        .iter()
        .flat_map(|r| r.frames.iter().map(|f| f.sequence))
        .collect();
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=10).collect::<Vec<_>>());
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.frames)
            .all(|f| f.receive_us != 0 && f.decode_us != 0 && f.present_us != 0)
    );
    let stats = stream.stats();
    assert_eq!(stats.decoded, 10);
    assert!(stats.feedback_sent >= 1);
    assert!(jobs.keyframe_requests().is_empty());

    let outcome = stream.close().await;
    assert_eq!(outcome, CloseOutcome::Closed);
    client.close();
    receive.await.unwrap();
    queue.quit();
}
