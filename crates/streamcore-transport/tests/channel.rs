use std::{sync::Arc, time::Duration};
use streamcore_media::{
    binary::{ClockPing, ClockPong, Frame},
    media::{
        ExtendedVideoPacket, FRAMES_V4_CAPABILITY, FrameVersion, VideoExtensions, VideoMetadata,
    },
    video_control::FrameTimingReport,
};
use streamcore_transport::{
    channel::{
        ChannelPath, ChannelTransport, SecureChannel, SecureListener, SessionOptions,
        connect_secure, listen_secure,
    },
    clock::{CLOCK_CAPABILITY, monotonic_now_us},
    identity::PeerIdentity,
};

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec!["inventory.v1".into()],
        required: vec!["inventory.v1".into()],
    }
}

fn clock_options() -> SessionOptions {
    SessionOptions {
        capabilities: vec!["inventory.v1".into(), CLOCK_CAPABILITY.into()],
        required: vec!["inventory.v1".into(), CLOCK_CAPABILITY.into()],
    }
}

async fn channels(
    transport: ChannelTransport,
) -> (Arc<SecureListener>, Arc<SecureChannel>, Arc<SecureChannel>) {
    channels_with(transport, options(), options()).await
}

async fn channels_with(
    transport: ChannelTransport,
    host_options: SessionOptions,
    client_options: SessionOptions,
) -> (Arc<SecureListener>, Arc<SecureChannel>, Arc<SecureChannel>) {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        host_options,
    )
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
        client_options,
        streamcore_transport::OperationCancellation::new(),
    )
    .await
    .unwrap();
    assert_eq!(connected.remote_address(), listener.address());
    (listener, server.await.unwrap(), connected)
}

#[tokio::test]
async fn pending_receive_does_not_block_send_and_close_releases_tcp() {
    let (_listener, server, client) = channels(ChannelTransport::TlsTcp).await;
    let receiving = client.clone();
    let pending = tokio::spawn(async move { receiving.receive().await });
    client.send(b"input".to_vec()).await.unwrap();
    assert_eq!(server.receive().await.unwrap(), b"input");
    server.send(b"output".to_vec()).await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap(), b"output");
    client.close();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), server.receive())
            .await
            .unwrap()
            .is_err()
    );
    assert!(client.send(b"late input".to_vec()).await.is_err());
}

#[tokio::test]
async fn cancellation_of_pending_receive_closes_channel_before_reuse() {
    let (_listener, server, client) = channels(ChannelTransport::Quic).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.receive())
            .await
            .is_err()
    );
    // Cancellation crosses the runtime boundary; wait for its close signal.
    assert!(
        tokio::time::timeout(Duration::from_secs(2), server.receive())
            .await
            .unwrap()
            .is_err()
    );
    assert!(client.send(b"never replay".to_vec()).await.is_err());
}

#[tokio::test]
async fn closing_listener_cancels_pending_accept() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host,
        vec![client.certificate()],
        options(),
    )
    .await
    .unwrap();
    let accepting = listener.clone();
    let pending = tokio::spawn(async move { accepting.accept().await });
    listener.close();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(listener.accept().await.is_err());
}

#[tokio::test]
async fn exhausted_bulk_credit_cannot_hold_up_control_on_either_transport() {
    use streamcore_transport::multiplex::ChannelLane;
    for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
        let (_listener, server, client) = channels(transport).await;
        let data = vec![0x5a; 8 * 1024 * 1024];
        let first = client.enqueue(ChannelLane::Bulk, data.clone()).unwrap();
        let second = client.enqueue(ChannelLane::Bulk, data.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), first.written())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), second.written())
            .await
            .unwrap()
            .unwrap();
        // No application reads: the full 16MiB bulk window is occupied. A third
        // transfer must remain queued, while input gets its own credit/stream.
        let third = client.enqueue(ChannelLane::Bulk, data.clone()).unwrap();
        let third_wait = third.written();
        tokio::pin!(third_wait);
        let input = client
            .enqueue(ChannelLane::Interactive, b"key-up".to_vec())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), input.written())
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut third_wait)
                .await
                .is_err()
        );
        let mut bulk = 0;
        let mut saw_input = false;
        for _ in 0..3 {
            let message = tokio::time::timeout(Duration::from_secs(2), server.receive())
                .await
                .unwrap()
                .unwrap();
            if message == b"key-up" {
                saw_input = true;
            } else {
                assert_eq!(message, data);
                bulk += 1;
            }
            if saw_input && bulk == 2 {
                break;
            }
        }
        assert!(saw_input);
        tokio::time::timeout(Duration::from_secs(10), &mut third_wait)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), server.receive())
                .await
                .unwrap()
                .unwrap(),
            data
        );
        client.close();
        server.close();
    }
}

#[tokio::test]
async fn closing_a_credit_stalled_lane_releases_every_pending_write() {
    use streamcore_transport::multiplex::ChannelLane;
    for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
        let (_listener, server, client) = channels(transport).await;
        let filled = client
            .enqueue(ChannelLane::Audio, vec![7; 2 * 1024 * 1024])
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), filled.written())
            .await
            .unwrap()
            .unwrap();
        let mut pending = Vec::new();
        for _ in 0..16 {
            pending.push(client.enqueue(ChannelLane::Audio, vec![1]).unwrap());
        }
        client.close();
        for receipt in pending {
            assert!(
                tokio::time::timeout(Duration::from_secs(1), receipt.written())
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        assert!(client.enqueue(ChannelLane::Interactive, vec![1]).is_err());
        // Closing drops actual socket halves while both channel handles survive.
        // A lost QUIC CONNECTION_CLOSE is resent only when the peer next sends,
        // at the latest its keep-alive two seconds later. On Windows 43 of 300
        // QUIC closes were observed after 2.004–2.032 s, never later; TLS/TCP
        // was always immediate. Allow more than one keep-alive interval.
        let limit = match transport {
            ChannelTransport::Quic => Duration::from_secs(5),
            ChannelTransport::TlsTcp => Duration::from_secs(2),
        };
        loop {
            if tokio::time::timeout(limit, server.receive())
                .await
                .unwrap_or_else(|_| {
                    let name = match transport {
                        ChannelTransport::Quic => "QUIC",
                        ChannelTransport::TlsTcp => "TLS/TCP",
                    };
                    panic!("{name} server did not observe the close within {limit:?}")
                })
                .is_err()
            {
                break;
            }
        }
    }
}

#[tokio::test]
async fn completed_goodbye_is_delivered_before_peer_close() {
    for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
        let (_listener, server, client) = channels(transport).await;
        // Explicitly synchronize the receiver before emulating a peer close.
        // Its completed frame must remain available even when the read driver
        // has already observed transport EOF by the next application poll.
        client.send(b"goodbye".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        client.close();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(server.receive().await.unwrap(), b"goodbye");
        assert!(server.receive().await.is_err());
    }
}

#[tokio::test]
async fn typed_pings_are_answered_by_the_core_and_estimate_a_shared_clock() {
    for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
        let (_listener, server, client) =
            channels_with(transport, clock_options(), clock_options()).await;
        assert!(client.clock_estimate().is_none());
        let before = monotonic_now_us();
        for _ in 0..5 {
            client.send_clock_ping().unwrap();
            // The host application sees the ping but does not answer it.
            let ping = server.receive().await.unwrap();
            let (frame, _) = Frame::read(&ping).unwrap().unwrap();
            assert_eq!(frame.kind, 2);
            let ping = ClockPing::decode(frame.payload).unwrap();
            assert!(ping.send_us >= before);
            let pong = client.receive().await.unwrap();
            let (frame, _) = Frame::read(&pong).unwrap().unwrap();
            assert_eq!(frame.kind, 3);
            let pong = ClockPong::decode(frame.payload).unwrap();
            assert_eq!(pong.nonce, ping.nonce);
            assert!(pong.receive_us >= ping.send_us && pong.send_us >= pong.receive_us);
        }
        let estimate = client.clock_estimate().unwrap();
        assert_eq!(estimate.samples, 5);
        // One process, one clock: the true offset is zero and lies in the bound.
        assert!(estimate.offset_us.unsigned_abs() <= estimate.error_us);
        assert!(estimate.rtt_us <= estimate.last_rtt_us.max(estimate.rtt_us));
        // Only the core may send pings and pongs under clock.v1.
        for kind in [2, 3] {
            let raw = Frame { kind, payload: &[] }.encode().unwrap();
            assert!(client.enqueue_application_frame(raw.clone()).is_err());
            assert!(server.enqueue_application_frame(raw).is_err());
        }
        // Either side may ping.
        server.send_clock_ping().unwrap();
        let ping = client.receive().await.unwrap();
        assert_eq!(ping[0], 2);
        assert_eq!(server.receive().await.unwrap()[0], 3);
        assert_eq!(server.clock_estimate().unwrap().samples, 1);
    }
}

#[tokio::test]
async fn without_clock_v1_pings_stay_application_frames() {
    let (_listener, server, client) = channels(ChannelTransport::TlsTcp).await;
    assert!(client.send_clock_ping().is_err());
    let empty = Frame {
        kind: 2,
        payload: &[],
    }
    .encode()
    .unwrap();
    client.enqueue_application_frame(empty.clone()).unwrap();
    assert_eq!(server.receive().await.unwrap(), empty);
    assert!(client.clock_estimate().is_none());
}

#[tokio::test]
async fn a_malformed_typed_ping_closes_the_channel() {
    let (_listener, server, client) =
        channels_with(ChannelTransport::TlsTcp, clock_options(), clock_options()).await;
    // The raw lane interface carries no frame check; the peer's core refuses it.
    let bad = Frame {
        kind: 2,
        payload: &[0; 3],
    }
    .encode()
    .unwrap();
    client
        .enqueue(
            streamcore_transport::multiplex::ChannelLane::Interactive,
            bad,
        )
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), server.receive())
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn clock_v1_is_a_hard_capability_break() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        clock_options(),
    )
    .await
    .unwrap();
    let accept = listener.clone();
    let _server = tokio::spawn(async move { accept.accept().await });
    let old = connect_secure(
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
    .await;
    assert!(old.is_err());
}

#[tokio::test]
async fn receive_timing_and_write_time_follow_the_monotonic_clock() {
    let (_listener, server, client) = channels(ChannelTransport::TlsTcp).await;
    let before = monotonic_now_us();
    // Three fragments, so the first and last arrivals can differ.
    let video = Frame {
        kind: 0x10,
        payload: &vec![7; 40 * 1024],
    }
    .encode()
    .unwrap();
    let pending = client.enqueue_application_frame(video.clone()).unwrap();
    assert!(pending.written_at_us().is_none());
    pending.written().await.unwrap();
    let written = pending.written_at_us().unwrap();
    assert!(written >= before);
    let message = server.receive_timed().await.unwrap();
    assert_eq!(message.payload, video);
    assert!(message.first_byte_us >= before);
    assert!(message.complete_us >= message.first_byte_us);
    assert!(monotonic_now_us() >= message.complete_us);
}

fn frames_options(v4: bool) -> SessionOptions {
    let mut capabilities = vec!["inventory.v1".to_string()];
    if v4 {
        capabilities.push(FRAMES_V4_CAPABILITY.into());
    }
    SessionOptions {
        capabilities,
        required: vec!["inventory.v1".into()],
    }
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    Frame { kind, payload }.encode().unwrap()
}

fn extended_video() -> Vec<u8> {
    let packet = ExtendedVideoPacket {
        metadata: VideoMetadata {
            stream_id: 1,
            generation: 1,
            sequence: 1,
            flags: 1,
            pts_micros: 1,
        },
        extensions: VideoExtensions {
            capture_us: Some(5),
            discardable: true,
            ..Default::default()
        },
        data: &[1, 2, 3],
    };
    frame(0x10, &packet.encode(FrameVersion::V4).unwrap())
}

fn timing() -> Vec<u8> {
    let body = FrameTimingReport {
        stream_id: 1,
        generation: 1,
        sequence: 1,
        h5_us: 1,
        h6_us: 2,
        backpressured: false,
    }
    .encode()
    .unwrap();
    frame(0x14, &body)
}

#[tokio::test]
async fn frames_v4_lane_zero_kinds_and_extensions_flow_only_when_negotiated() {
    for transport in [ChannelTransport::TlsTcp, ChannelTransport::Quic] {
        let (_listener, server, client) =
            channels_with(transport, frames_options(true), frames_options(true)).await;
        assert!(server.frames_v4() && client.frames_v4());
        server.enqueue_application_frame(timing()).unwrap();
        server.enqueue_application_frame(extended_video()).unwrap();
        let mut received = vec![client.receive().await.unwrap()];
        received.push(client.receive().await.unwrap());
        received.sort();
        assert_eq!(received, {
            let mut expected = vec![timing(), extended_video()];
            expected.sort();
            expected
        });
        let video = extended_video();
        let decoded = Frame::read(&video).unwrap().unwrap().0;
        let packet = ExtendedVideoPacket::decode(decoded.payload, FrameVersion::V4).unwrap();
        assert!(packet.extensions.discardable);
        // A malformed lane-0 video body is refused before it is sent.
        assert!(
            server
                .enqueue_application_frame(frame(0x14, &[0; 3]))
                .is_err()
        );

        // One side without frames.v4: neither may send the new kinds or the flag.
        let (_listener, server, client) =
            channels_with(transport, frames_options(true), frames_options(false)).await;
        assert!(!server.frames_v4() && !client.frames_v4());
        assert!(server.enqueue_application_frame(timing()).is_err());
        assert!(server.enqueue_application_frame(extended_video()).is_err());
        // A peer that sends one anyway (bypassing the check) is disconnected.
        server
            .enqueue(
                streamcore_transport::multiplex::ChannelLane::Interactive,
                timing(),
            )
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), client.receive())
                .await
                .unwrap()
                .is_err()
        );
    }
}
