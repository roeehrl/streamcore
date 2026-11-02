use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use streamcore_transport::{
    identity::PeerIdentity,
    protocol::{Hello, PROTOCOL_MAJOR},
    transport::{DialPath, SessionListener, TransportKind, dial},
};

fn hello() -> Hello {
    Hello {
        major: PROTOCOL_MAJOR,
        capabilities: vec!["inventory.v1".into(), "terminal.v1".into()],
        required: vec!["inventory.v1".into()],
    }
}

fn path(address: std::net::SocketAddr, kind: TransportKind) -> DialPath {
    DialPath {
        address,
        kind,
        delay: Duration::ZERO,
    }
}

async fn round_trip(kind: TransportKind) {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    let expected = client.fingerprint();
    let server = tokio::spawn(async move {
        let mut session = listener.accept().await.unwrap();
        assert_eq!(session.peer_fingerprint, expected);
        assert_eq!(session.receive().await.unwrap(), b"private terminal bytes");
        session.send(b"acknowledged").await.unwrap();
        // Keep the QUIC endpoint alive through acknowledgement consumption.
        let _ = session.receive().await;
    });
    let mut session = dial(&client, host.certificate(), vec![path(addr, kind)], hello())
        .await
        .unwrap();
    assert_eq!(session.kind, kind);
    assert_eq!(session.peer_fingerprint, host.fingerprint());
    assert_eq!(session.capabilities, hello().capabilities);
    session.send(b"private terminal bytes").await.unwrap();
    assert_eq!(session.receive().await.unwrap(), b"acknowledged");
    drop(session);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn quic_mutually_authenticates_and_exchanges_private_frames() {
    round_trip(TransportKind::Quic).await;
}

#[tokio::test]
async fn tcp_mutually_authenticates_and_exchanges_private_frames() {
    round_trip(TransportKind::TlsTcp).await;
}

#[test]
fn restored_identity_retains_pin_and_rejects_corruption_or_mismatched_key() {
    let identity = PeerIdentity::generate().unwrap();
    let saved = identity.export_for_secure_storage().unwrap();
    let restored = PeerIdentity::restore(saved.clone()).unwrap();
    assert_eq!(restored.certificate(), identity.certificate());
    assert_eq!(restored.fingerprint(), identity.fingerprint());
    assert!(PeerIdentity::restore(vec![0; 65_536]).is_err());
    let other = PeerIdentity::generate().unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    let other: serde_json::Value =
        serde_json::from_slice(&other.export_for_secure_storage().unwrap()).unwrap();
    changed["private_key"] = other["private_key"].clone();
    assert!(PeerIdentity::restore(serde_json::to_vec(&changed).unwrap()).is_err());
    changed["version"] = 2.into();
    assert!(PeerIdentity::restore(serde_json::to_vec(&changed).unwrap()).is_err());
}

async fn rejects_wrong_peer(kind: TransportKind, wrong_server: bool) {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let stranger = PeerIdentity::generate().unwrap();
    let trusted_client = if wrong_server {
        client.certificate()
    } else {
        stranger.certificate()
    };
    let expected_server = if wrong_server {
        stranger.certificate()
    } else {
        host.certificate()
    };
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![trusted_client],
        hello(),
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        assert!(listener.accept().await.is_err());
    });
    let started = Instant::now();
    assert!(
        dial(&client, expected_server, vec![path(addr, kind)], hello())
            .await
            .is_err()
    );
    if kind == TransportKind::TlsTcp {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "identity rejection was retried"
        );
    }
    tokio::time::timeout(Duration::from_secs(7), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn quic_rejects_an_unknown_client() {
    rejects_wrong_peer(TransportKind::Quic, false).await;
}
#[tokio::test]
async fn tcp_rejects_an_unknown_client() {
    rejects_wrong_peer(TransportKind::TlsTcp, false).await;
}
#[tokio::test]
async fn quic_rejects_a_replaced_host_identity() {
    rejects_wrong_peer(TransportKind::Quic, true).await;
}
#[tokio::test]
async fn tcp_rejects_a_replaced_host_identity() {
    rejects_wrong_peer(TransportKind::TlsTcp, true).await;
}

#[tokio::test]
async fn unreachable_udp_falls_back_without_waiting_for_quic_timeout() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    // A bound UDP socket that consumes no packets models a black hole, without
    // changing the machine firewall or relying on a public network endpoint.
    let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let server = tokio::spawn(async move {
        let mut accepted = listener.accept().await.unwrap();
        assert_eq!(accepted.receive().await.unwrap(), b"fallback");
        accepted.send(b"ok").await.unwrap();
        let _ = accepted.receive().await;
    });
    let started = Instant::now();
    let mut session = dial(
        &client,
        host.certificate(),
        vec![
            path(blackhole.local_addr().unwrap(), TransportKind::Quic),
            DialPath {
                address: addr,
                kind: TransportKind::TlsTcp,
                delay: Duration::from_millis(150),
            },
        ],
        hello(),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "fallback waited for the five-second QUIC timeout"
    );
    assert_eq!(session.kind, TransportKind::TlsTcp);
    session.send(b"fallback").await.unwrap();
    assert_eq!(session.receive().await.unwrap(), b"ok");
    drop(session);
    server.await.unwrap();
}

#[tokio::test]
async fn recovered_tcp_retries_while_a_quic_blackhole_is_pending() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let address = listener.local_addr().unwrap();
    let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gate_address = gate.local_addr().unwrap();
    let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let started = Instant::now();
    let bridge = tokio::spawn(async move {
        let mut rejected = 0;
        loop {
            let (mut incoming, _) = gate.accept().await.unwrap();
            if started.elapsed() < Duration::from_millis(700) {
                rejected += 1;
                drop(incoming);
                continue;
            }
            let mut outgoing = tokio::net::TcpStream::connect(address).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
            assert!(
                rejected > 0,
                "the fixture did not exercise an unavailable TCP path"
            );
            break;
        }
    });
    let server = tokio::spawn(async move {
        let mut accepted = listener.accept().await.unwrap();
        assert_eq!(
            accepted.receive().await.unwrap(),
            b"one application message"
        );
        accepted.send(b"one reply").await.unwrap();
        assert!(accepted.receive().await.is_err());
    });
    let mut connected = dial(
        &client,
        host.certificate(),
        vec![
            path(blackhole.local_addr().unwrap(), TransportKind::Quic),
            DialPath {
                address: gate_address,
                kind: TransportKind::TlsTcp,
                delay: Duration::from_millis(250),
            },
        ],
        hello(),
    )
    .await
    .expect("a recovered TCP path was discarded while QUIC remained pending");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(connected.kind, TransportKind::TlsTcp);
    connected.send(b"one application message").await.unwrap();
    assert_eq!(connected.receive().await.unwrap(), b"one reply");
    drop(connected);
    bridge.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_a_tcp_race_stops_further_retry_connections() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = gate.local_addr().unwrap();
    let task = tokio::spawn(async move {
        dial(
            &client,
            host.certificate(),
            vec![path(address, TransportKind::TlsTcp)],
            hello(),
        )
        .await
    });
    // Receiving a second socket establishes that the actual retry path ran.
    for _ in 0..2 {
        let (connection, _) = tokio::time::timeout(Duration::from_secs(2), gate.accept())
            .await
            .unwrap()
            .unwrap();
        drop(connection);
    }
    task.abort();
    assert!(task.await.is_err_and(|error| error.is_cancelled()));
    assert!(
        tokio::time::timeout(Duration::from_millis(750), gate.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tcp_retries_cannot_extend_the_original_handshake_deadline() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = gate.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let listener = tokio::spawn(async move {
        loop {
            let (connection, _) = gate.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            drop(connection);
        }
    });
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(6),
        dial(
            &client,
            host.certificate(),
            vec![path(address, TransportKind::TlsTcp)],
            hello(),
        ),
    )
    .await;
    listener.abort();
    let _ = listener.await;
    assert!(
        result
            .expect("retries extended the original deadline")
            .is_err()
    );
    assert!(started.elapsed() >= Duration::from_millis(4800));
    assert!((2..=16).contains(&attempts.load(Ordering::SeqCst)));
}

#[tokio::test]
async fn quic_rebind_preserves_the_authenticated_connection() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut session = listener.accept().await.unwrap();
        for _ in 0..2 {
            let bytes = session.receive().await.unwrap();
            session.send(&bytes).await.unwrap();
        }
        let _ = session.receive().await;
    });
    let mut session = dial(
        &client,
        host.certificate(),
        vec![path(addr, TransportKind::Quic)],
        hello(),
    )
    .await
    .unwrap();
    session.send(b"before").await.unwrap();
    assert_eq!(session.receive().await.unwrap(), b"before");
    session.rebind("127.0.0.1:0".parse().unwrap()).unwrap();
    session.send(b"after").await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), session.receive())
            .await
            .unwrap()
            .unwrap(),
        b"after"
    );
    assert_eq!(session.peer_fingerprint, host.fingerprint());
    drop(session);
    server.await.unwrap();
}

#[tokio::test]
async fn two_live_hosts_remain_independent_when_one_shuts_down() {
    let client = PeerIdentity::generate().unwrap();
    let mut servers = Vec::new();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let identity = PeerIdentity::generate().unwrap();
        let listener = Arc::new(
            SessionListener::bind(
                "127.0.0.1:0".parse().unwrap(),
                &identity,
                vec![client.certificate()],
                hello(),
            )
            .await
            .unwrap(),
        );
        let addr = listener.local_addr().unwrap();
        servers.push(tokio::spawn(async move {
            let mut session = listener.accept().await.unwrap();
            while let Ok(bytes) = session.receive().await {
                if session.send(&bytes).await.is_err() {
                    break;
                }
            }
        }));
        sessions.push(
            dial(
                &client,
                identity.certificate(),
                vec![path(addr, TransportKind::Quic)],
                hello(),
            )
            .await
            .unwrap(),
        );
    }
    assert_ne!(sessions[0].peer_fingerprint, sessions[1].peer_fingerprint);
    servers[0].abort();
    for i in 0..100_u32 {
        sessions[1].send(&i.to_be_bytes()).await.unwrap();
        assert_eq!(sessions[1].receive().await.unwrap(), i.to_be_bytes());
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(2), sessions[0].receive())
            .await
            .unwrap()
            .is_err()
    );
    drop(sessions);
    for server in servers {
        let _ = server.await;
    }
}

#[tokio::test]
async fn incompatible_capabilities_fail_before_application_data() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        assert!(listener.accept().await.is_err());
    });
    let unsupported = Hello {
        major: streamcore_transport::protocol::PROTOCOL_MAJOR,
        capabilities: vec!["other.v1".into()],
        required: vec!["other.v1".into()],
    };
    assert!(
        dial(
            &client,
            host.certificate(),
            vec![path(addr, TransportKind::TlsTcp)],
            unsupported
        )
        .await
        .is_err()
    );
    server.await.unwrap();
}

#[test]
fn security_configs_disable_early_data_and_require_client_auth() {
    let a = PeerIdentity::generate().unwrap();
    let b = PeerIdentity::generate().unwrap();
    assert!(!a.client_config(b.certificate()).unwrap().enable_early_data);
    let server = b.server_config(vec![a.certificate()]).unwrap();
    assert_eq!(server.max_early_data_size, 0);
    assert_eq!(server.send_tls13_tickets, 0);
    assert!(a.server_config(vec![]).is_ok()); // Bound listener, no approved peers.
    assert!(a.client_config(vec![]).is_err());
}

#[tokio::test]
async fn stalled_unauthenticated_socket_does_not_block_another_client() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let address = listener.local_addr().unwrap();
    let _stalled = tokio::net::TcpStream::connect(address).await.unwrap();
    let server = tokio::spawn(async move {
        let mut session = listener.accept().await.unwrap();
        assert_eq!(session.receive().await.unwrap(), b"not blocked");
        session.send(b"ok").await.unwrap();
        let _ = session.receive().await;
    });
    let mut session = tokio::time::timeout(
        Duration::from_secs(2),
        dial(
            &client,
            host.certificate(),
            vec![path(address, TransportKind::TlsTcp)],
            hello(),
        ),
    )
    .await
    .expect("a stalled handshake blocked a legitimate client")
    .unwrap();
    session.send(b"not blocked").await.unwrap();
    assert_eq!(session.receive().await.unwrap(), b"ok");
    drop(session);
    server.await.unwrap();
}

#[tokio::test]
async fn mismatched_alpn_is_rejected_before_protocol_negotiation() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        assert!(listener.accept().await.is_err());
    });
    let mut config = (*client.client_config(host.certificate()).unwrap()).clone();
    config.alpn_protocols = vec![b"other-protocol/1".to_vec()];
    let socket = tokio::net::TcpStream::connect(address).await.unwrap();
    assert!(
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(
                rustls::pki_types::ServerName::try_from("peer.mycnc.invalid").unwrap(),
                socket,
            )
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn a_client_without_a_certificate_cannot_open_a_session() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    for trusted in [vec![], vec![client.certificate()]] {
        let listener =
            SessionListener::bind("127.0.0.1:0".parse().unwrap(), &host, trusted, hello())
                .await
                .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            assert!(listener.accept().await.is_err());
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(host.certificate().into()).unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"mycnc/2".to_vec()];
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let connection = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(
                rustls::pki_types::ServerName::try_from("peer.mycnc.invalid").unwrap(),
                socket,
            )
            .await;
        // TLS 1.3 clients can finish their side before receiving the server's
        // certificate-required alert. Neither outcome may yield application data.
        if let Ok(mut stream) = connection {
            assert!(
                streamcore_transport::protocol::read_frame(&mut stream)
                    .await
                    .is_err()
            );
        }
        server.await.unwrap();
    }
}
