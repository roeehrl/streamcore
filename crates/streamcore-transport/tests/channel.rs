use std::{sync::Arc, time::Duration};
use streamcore_transport::{
    channel::{
        ChannelPath, ChannelTransport, SecureChannel, SecureListener, SessionOptions,
        connect_secure, listen_secure,
    },
    identity::PeerIdentity,
};

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec!["inventory.v1".into()],
        required: vec!["inventory.v1".into()],
    }
}

async fn channels(
    transport: ChannelTransport,
) -> (Arc<SecureListener>, Arc<SecureChannel>, Arc<SecureChannel>) {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        options(),
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
        options(),
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
