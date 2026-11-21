//! Sessions over a pipe a USB provider opened (STREAMCORE-VIDEO 8.2): the same
//! pinned mutual TLS, hello, admission and lanes as TLS/TCP, on a byte stream
//! that is not a socket. An in-memory duplex stands in for the provider's pipe.

use std::{sync::Arc, time::Duration};
use streamcore_transport::{
    OperationCancellation,
    channel::{
        ChannelLocator, ChannelTransport, SessionOptions, UsbPipe, connect_secure_stream_with,
        listen_secure,
    },
    identity::PeerIdentity,
    multiplex::binary_lanes,
    protocol::{Hello, PROTOCOL_MAJOR},
    transport::{Medium, PeerLocator, SessionListener, TransportError, TransportKind, dial_pipe},
};

fn hello() -> Hello {
    Hello {
        major: PROTOCOL_MAJOR,
        capabilities: vec!["inventory.v1".into()],
        required: vec![],
    }
}

fn usb(device: &str) -> PeerLocator {
    PeerLocator::Usb {
        medium: Medium::UsbAdb,
        device: device.into(),
    }
}

async fn listener(host: &PeerIdentity, client: &PeerIdentity) -> SessionListener {
    SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_session_runs_over_a_pipe_with_the_same_pins() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = listener(&host, &client).await;
    let (host_end, device_end) = tokio::io::duplex(64 * 1024);
    listener
        .accept_stream(Box::new(host_end), usb("device-1"))
        .unwrap();
    let expected = client.fingerprint();
    let server = tokio::spawn(async move {
        let mut session = listener.accept().await.unwrap();
        assert_eq!(session.peer_fingerprint, expected);
        assert_eq!(
            session.kind,
            TransportKind::TlsStream {
                medium: Medium::UsbAdb
            }
        );
        assert_eq!(session.locator, usb("device-1"));
        assert_eq!(session.receive().await.unwrap(), b"over the cable");
        session.send(b"acknowledged").await.unwrap();
    });
    let mut session = dial_pipe(
        &client,
        host.certificate(),
        Box::new(device_end),
        usb("host"),
        hello(),
    )
    .await
    .unwrap();
    assert_eq!(session.peer_fingerprint, host.fingerprint());
    assert_eq!(session.capabilities, hello().capabilities);
    session.send(b"over the cable").await.unwrap();
    assert_eq!(session.receive().await.unwrap(), b"acknowledged");
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_cable_is_not_authentication() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let stranger = PeerIdentity::generate().unwrap();
    let listener = listener(&host, &client).await;

    // An unpaired device on the cable: the host refuses its certificate.
    let (host_end, device_end) = tokio::io::duplex(64 * 1024);
    listener
        .accept_stream(Box::new(host_end), usb("device-1"))
        .unwrap();
    let dialed = dial_pipe(
        &stranger,
        host.certificate(),
        Box::new(device_end),
        usb("host"),
        hello(),
    )
    .await;
    let accepted = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(
        matches!(accepted, Ok(Err(_))),
        "the host accepted a stranger"
    );
    // TLS 1.3 reports the client-certificate rejection after the client's
    // handshake, so the dial fails here or on its first read.
    if let Ok(mut session) = dialed {
        assert!(session.receive().await.is_err());
    }

    // A different computer on the cable: the paired device refuses it.
    let impostor = PeerIdentity::generate().unwrap();
    let other = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &impostor,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let (host_end, device_end) = tokio::io::duplex(64 * 1024);
    other
        .accept_stream(Box::new(host_end), usb("device-1"))
        .unwrap();
    let dialed = dial_pipe(
        &client,
        host.certificate(),
        Box::new(device_end),
        usb("host"),
        hello(),
    )
    .await;
    assert!(dialed.is_err(), "the device accepted an unpinned host");
}

#[tokio::test]
async fn a_device_holds_at_most_two_pending_handshakes() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = listener(&host, &client).await;
    // Three pipes from one device that never speak, then one from another.
    let mut stalled = Vec::new();
    for _ in 0..3 {
        let (host_end, device_end) = tokio::io::duplex(1024);
        listener
            .accept_stream(Box::new(host_end), usb("stalling"))
            .unwrap();
        stalled.push(device_end);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The third was dropped by the listener: its device end reads end-of-file.
    let mut third = stalled.pop().unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::io::AsyncReadExt::read(&mut third, &mut byte),
    )
    .await
    .expect("the third pipe was not closed");
    assert_eq!(read.unwrap(), 0);

    // Another device still connects while those two stall.
    let (host_end, device_end) = tokio::io::duplex(64 * 1024);
    listener
        .accept_stream(Box::new(host_end), usb("device-2"))
        .unwrap();
    let dial = tokio::spawn(async move {
        dial_pipe(
            &client,
            host.certificate(),
            Box::new(device_end),
            usb("host"),
            hello(),
        )
        .await
    });
    let session = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("the stalled device blocked another device")
        .unwrap();
    assert_eq!(session.locator, usb("device-2"));
    dial.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_socket_locator_or_a_pipe_kind_in_a_path_set_is_refused() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let listener = listener(&host, &client).await;
    let (host_end, _device_end) = tokio::io::duplex(1024);
    let socket = PeerLocator::Socket("127.0.0.1:9".parse().unwrap());
    assert!(matches!(
        listener.accept_stream(Box::new(host_end), socket),
        Err(TransportError::InvalidPaths)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secure_channel_over_a_pipe_carries_messages_and_reports_its_locator() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let options = SessionOptions {
        capabilities: vec![],
        required: vec![],
    };
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        options.clone(),
    )
    .await
    .unwrap();
    let (host_end, device_end) = tokio::io::duplex(64 * 1024);
    listener
        .accept_stream(UsbPipe {
            io: Box::new(host_end),
            medium: Medium::UsbMux,
            device: "device-1".into(),
        })
        .unwrap();
    let accepting = listener.clone();
    let server = tokio::spawn(async move { accepting.accept().await.unwrap() });
    let channel = connect_secure_stream_with(
        binary_lanes,
        client,
        host.certificate(),
        UsbPipe {
            io: Box::new(device_end),
            medium: Medium::UsbMux,
            device: "host".into(),
        },
        options,
        OperationCancellation::new(),
    )
    .await
    .unwrap();
    let server = server.await.unwrap();
    let over_usbmux = ChannelTransport::TlsStream {
        medium: Medium::UsbMux,
    };
    assert_eq!(server.transport(), over_usbmux);
    assert_eq!(channel.transport(), over_usbmux);
    assert_eq!(
        server.locator(),
        ChannelLocator::Usb {
            medium: Medium::UsbMux,
            device: "device-1".into()
        }
    );
    assert_eq!(server.remote_address(), "");
    channel
        .send(b"hello over the cable".to_vec())
        .await
        .unwrap();
    assert_eq!(server.receive().await.unwrap(), b"hello over the cable");
    server.send(b"hello back".to_vec()).await.unwrap();
    assert_eq!(channel.receive().await.unwrap(), b"hello back");
}
