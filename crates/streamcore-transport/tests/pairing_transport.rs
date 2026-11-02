use std::{sync::Arc, time::Duration};
use streamcore_transport::{
    channel::{ChannelPath, ChannelTransport, SessionOptions, connect_secure, listen_secure},
    identity::PeerIdentity,
    pairing_transport::{PairingNetworkError, listen_pairing, pair_with_host},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[tokio::test]
async fn network_pairing_waits_for_storage_then_opens_mutually_trusted_channels() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host.clone())
        .await
        .unwrap();
    let pairing = tokio::spawn(pair_with_host(
        vec![service.address()],
        client.clone(),
        service.display_code().unwrap(),
        Some(host.fingerprint()),
        streamcore_transport::OperationCancellation::new(),
    ));
    let pending = tokio::time::timeout(Duration::from_secs(3), service.next_verified())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.peer_certificate(), client.certificate());
    assert!(
        !pairing.is_finished(),
        "client released a trusted pin before host storage succeeded"
    );
    // A real native owner persists here. This test deliberately controls the
    // acknowledgement instead of treating cryptographic verification as saving.
    pending.confirm_persisted().await.unwrap();
    service.close(); // must not discard the confirmation that was just written
    let pin = pairing.await.unwrap().unwrap();
    assert_eq!(pin, host.certificate());
    assert!(pending.confirm_persisted().await.is_err());
    for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
        let options = SessionOptions {
            capabilities: vec!["probe.v1".into()],
            required: vec![],
        };
        let listener = listen_secure(
            "127.0.0.1:0".into(),
            host.clone(),
            vec![pending.peer_certificate()],
            options.clone(),
        )
        .await
        .unwrap();
        let accepting = listener.clone();
        let server = tokio::spawn(async move { accepting.accept().await.unwrap() });
        let connected = connect_secure(
            client.clone(),
            pin.clone(),
            vec![ChannelPath {
                address: listener.address(),
                transport,
                delay_ms: 0,
            }],
            options,
            streamcore_transport::OperationCancellation::new(),
        )
        .await
        .unwrap();
        let server = server.await.unwrap();
        connected
            .send(b"paired over a socket".to_vec())
            .await
            .unwrap();
        assert_eq!(server.receive().await.unwrap(), b"paired over a socket");
        connected.close();
        server.close();
        listener.close();
    }
}

#[tokio::test]
async fn failed_or_abandoned_host_storage_never_releases_a_pin() {
    for explicit in [true, false] {
        let host = Arc::new(PeerIdentity::generate().unwrap());
        let client = Arc::new(PeerIdentity::generate().unwrap());
        let service = listen_pairing("127.0.0.1:0".into(), host).await.unwrap();
        let pairing = tokio::spawn(pair_with_host(
            vec![service.address()],
            client,
            service.display_code().unwrap(),
            None,
            streamcore_transport::OperationCancellation::new(),
        ));
        let pending = service.next_verified().await.unwrap();
        if explicit {
            pending.reject_persistence().unwrap();
        }
        drop(pending);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), pairing)
                .await
                .unwrap()
                .unwrap(),
            Err(PairingNetworkError::Persistence)
        ));
        service.close();
    }
}

#[tokio::test]
async fn wrong_code_consumes_the_window_budget_across_fresh_sockets() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host).await.unwrap();
    let correct = service.display_code().unwrap();
    let wrong = if correct == "000000" {
        "111111"
    } else {
        "000000"
    };
    for _ in 0..5 {
        assert!(
            pair_with_host(
                vec![service.address()],
                client.clone(),
                wrong.into(),
                None,
                streamcore_transport::OperationCancellation::new()
            )
            .await
            .is_err()
        );
    }
    assert!(
        pair_with_host(
            vec![service.address()],
            client,
            correct,
            None,
            streamcore_transport::OperationCancellation::new()
        )
        .await
        .is_err()
    );
    service.close();
}

#[tokio::test]
async fn wrong_invitation_pin_is_rejected_before_authentication() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host).await.unwrap();
    assert!(matches!(
        pair_with_host(
            vec![service.address()],
            client,
            service.display_code().unwrap(),
            Some("wrong".into()),
            streamcore_transport::OperationCancellation::new()
        )
        .await,
        Err(PairingNetworkError::WrongHost)
    ));
    service.close();
}

#[tokio::test]
async fn closing_pairing_cancels_storage_wait_and_closes_client_socket() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host).await.unwrap();
    let pairing = tokio::spawn(pair_with_host(
        vec![service.address()],
        client,
        service.display_code().unwrap(),
        None,
        streamcore_transport::OperationCancellation::new(),
    ));
    let pending = service.next_verified().await.unwrap();
    service.close();
    assert!(pending.confirm_persisted().await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), pairing)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(service.next_verified().await.is_err());
    assert!(service.display_code().is_err());
}

#[tokio::test]
async fn oversized_bootstrap_frames_are_rejected_before_body_allocation() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host).await.unwrap();
    let code = service.display_code().unwrap();
    for _ in 0..5 {
        let mut socket = TcpStream::connect(service.address()).await.unwrap();
        let size = socket.read_u32().await.unwrap();
        let mut hello = vec![0; size as usize];
        socket.read_exact(&mut hello).await.unwrap();
        socket.write_u32(u32::MAX).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
    assert!(
        pair_with_host(
            vec![service.address()],
            client,
            code,
            None,
            streamcore_transport::OperationCancellation::new()
        )
        .await
        .is_err()
    );
    service.close();
}

#[tokio::test]
async fn closing_window_waits_for_socket_release_before_same_port_reopens() {
    let identity = Arc::new(PeerIdentity::generate().unwrap());
    let mut service = listen_pairing("127.0.0.1:0".into(), identity.clone())
        .await
        .unwrap();
    let address = service.address();
    for _ in 0..20 {
        let (one, two) = tokio::join!(service.close_and_wait(), service.close_and_wait());
        one.unwrap();
        two.unwrap();
        service = listen_pairing(address.clone(), identity.clone())
            .await
            .unwrap();
        assert!(service.display_code().is_ok());
    }
    service.close_and_wait().await.unwrap();
}

#[tokio::test]
async fn abandoned_public_hellos_do_not_consume_the_pin_budget() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host).await.unwrap();
    for _ in 0..8 {
        let mut socket = TcpStream::connect(service.address()).await.unwrap();
        let size = socket.read_u32().await.unwrap();
        let mut hello = vec![0; size as usize];
        socket.read_exact(&mut hello).await.unwrap();
        socket.shutdown().await.unwrap();
        // EOF confirms this handler released its connection slot.
        assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
    }
    let pairing = tokio::spawn(pair_with_host(
        vec![service.address()],
        client,
        service.display_code().unwrap(),
        None,
        streamcore_transport::OperationCancellation::new(),
    ));
    let pending = tokio::time::timeout(Duration::from_secs(2), service.next_verified())
        .await
        .unwrap()
        .unwrap();
    pending.confirm_persisted().await.unwrap();
    assert!(pairing.await.unwrap().is_ok());
    service.close();
}

#[tokio::test]
async fn a_silent_first_path_does_not_block_a_reachable_pairing_host() {
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host.clone())
        .await
        .unwrap();
    let pairing = tokio::spawn(pair_with_host(
        vec![silent.local_addr().unwrap().to_string(), service.address()],
        client,
        service.display_code().unwrap(),
        Some(host.fingerprint()),
        streamcore_transport::OperationCancellation::new(),
    ));
    let (mut abandoned, _) = silent.accept().await.unwrap();
    let pending = tokio::time::timeout(Duration::from_secs(2), service.next_verified())
        .await
        .unwrap()
        .unwrap();
    pending.confirm_persisted().await.unwrap();
    assert_eq!(pairing.await.unwrap().unwrap(), host.certificate());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), abandoned.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    service.close();
}

#[tokio::test]
async fn a_wrong_code_is_not_replayed_to_another_pairing_path() {
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let first = listen_pairing(
        "127.0.0.1:0".into(),
        Arc::new(PeerIdentity::generate().unwrap()),
    )
    .await
    .unwrap();
    let second = listen_pairing(
        "127.0.0.1:0".into(),
        Arc::new(PeerIdentity::generate().unwrap()),
    )
    .await
    .unwrap();
    let correct = second.display_code().unwrap();
    let wrong = (0..10)
        .map(|n| format!("{n:06}"))
        .find(|s| s != &correct && s != &first.display_code().unwrap())
        .unwrap();
    assert!(
        pair_with_host(
            vec![first.address(), second.address()],
            client.clone(),
            wrong.clone(),
            None,
            streamcore_transport::OperationCancellation::new()
        )
        .await
        .is_err()
    );
    // Four further wrong requests still leave one attempt on the other host.
    // An automatic replay of the earlier request would exhaust its budget.
    for _ in 0..4 {
        assert!(
            pair_with_host(
                vec![second.address()],
                client.clone(),
                wrong.clone(),
                None,
                streamcore_transport::OperationCancellation::new()
            )
            .await
            .is_err()
        );
    }
    let pairing = tokio::spawn(pair_with_host(
        vec![second.address()],
        client,
        correct,
        None,
        streamcore_transport::OperationCancellation::new(),
    ));
    let pending = tokio::time::timeout(Duration::from_secs(2), second.next_verified())
        .await
        .unwrap()
        .unwrap();
    pending.confirm_persisted().await.unwrap();
    assert!(pairing.await.unwrap().is_ok());
    first.close();
    second.close();
}

#[tokio::test]
async fn pairing_path_input_is_bounded_and_validated_before_connecting() {
    let identity = Arc::new(PeerIdentity::generate().unwrap());
    for paths in [
        vec![],
        vec!["127.0.0.1:12345".into(); 17],
        vec!["0.0.0.0:12345".into()],
        vec!["[::1]:0".into()],
    ] {
        assert!(matches!(
            pair_with_host(
                paths,
                identity.clone(),
                "123456".into(),
                None,
                streamcore_transport::OperationCancellation::new()
            )
            .await,
            Err(PairingNetworkError::InvalidMessage)
        ));
    }
}
