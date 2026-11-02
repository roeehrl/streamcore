use std::{sync::Arc, time::Duration};
use streamcore_transport::{
    OperationCancellation,
    channel::{ChannelError, ChannelPath, ChannelTransport, SessionOptions, connect_secure},
    identity::PeerIdentity,
    pairing_transport::{PairingNetworkError, pair_with_host},
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
};

async fn closed(mut stream: TcpStream) {
    let mut bytes = [0; 4096];
    while stream.read(&mut bytes).await.unwrap() != 0 {}
}

#[tokio::test]
async fn explicit_cancellation_drops_a_pending_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cancel = OperationCancellation::new();
    let attempt = tokio::spawn(connect_secure(
        Arc::new(PeerIdentity::generate().unwrap()),
        PeerIdentity::generate().unwrap().certificate(),
        vec![ChannelPath {
            address: listener.local_addr().unwrap().to_string(),
            transport: ChannelTransport::TlsTcp,
            delay_ms: 0,
        }],
        SessionOptions {
            capabilities: vec![],
            required: vec![],
        },
        cancel.clone(),
    ));
    let (stream, _) = listener.accept().await.unwrap();
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), attempt)
            .await
            .unwrap()
            .unwrap(),
        Err(ChannelError::Closed)
    ));
    tokio::time::timeout(Duration::from_secs(1), closed(stream))
        .await
        .unwrap();
}

#[tokio::test]
async fn explicit_cancellation_drops_pairing_before_a_public_hello() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cancel = OperationCancellation::new();
    let attempt = tokio::spawn(pair_with_host(
        vec![listener.local_addr().unwrap().to_string()],
        Arc::new(PeerIdentity::generate().unwrap()),
        "123456".into(),
        None,
        cancel.clone(),
    ));
    let (mut stream, _) = listener.accept().await.unwrap();
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), attempt)
            .await
            .unwrap()
            .unwrap(),
        Err(PairingNetworkError::Closed)
    ));
    // No PAKE request was written to this losing/cancelled socket.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}
