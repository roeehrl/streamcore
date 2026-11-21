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
    clock::CLOCK_CAPABILITY,
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

    // The same label on another medium is another device, with its own bound.
    let (host_end, mut other_medium) = tokio::io::duplex(1024);
    let same_label = PeerLocator::Usb {
        medium: Medium::UsbMux,
        device: "stalling".into(),
    };
    listener
        .accept_stream(Box::new(host_end), same_label)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let still_open = tokio::time::timeout(
        Duration::from_millis(100),
        tokio::io::AsyncReadExt::read(&mut other_medium, &mut byte),
    )
    .await;
    assert!(still_open.is_err(), "another medium shared the bound");

    // Another device still connects while those stall.
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
        capabilities: vec![CLOCK_CAPABILITY.into()],
        required: vec![CLOCK_CAPABILITY.into()],
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
    // The probe a client runs on a new pipe before it moves a session onto it.
    let probe = channel.probe(5, 20, 500).await.unwrap();
    assert_eq!((probe.sent, probe.answered), (5, 5));
    assert!(probe.rtt_p50_us.unwrap() < 100_000);
    // Pings and pongs are also delivered, as liveness signals: skip them.
    async fn next(channel: &streamcore_transport::channel::SecureChannel) -> Vec<u8> {
        loop {
            let message = channel.receive().await.unwrap();
            if !matches!(message.first(), Some(2 | 3)) {
                break message;
            }
        }
    }
    channel
        .send(b"hello over the cable".to_vec())
        .await
        .unwrap();
    assert_eq!(next(&server).await, b"hello over the cable");
    server.send(b"hello back".to_vec()).await.unwrap();
    assert_eq!(next(&channel).await, b"hello back");
}

/// A relay between two in-memory pipes that stops moving bytes when told to,
/// while keeping both ends open: the half-open state a real relay can be in
/// after an unplug. `capacity` is each side's buffer and `chunk_delay` a wait
/// after every 16 KiB moved, to stand for a real relay's buffers and rate.
fn stalling_relay(
    capacity: usize,
    chunk_delay: Duration,
) -> (
    tokio::io::DuplexStream,
    tokio::io::DuplexStream,
    tokio::sync::watch::Sender<bool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (host_end, mut host_side) = tokio::io::duplex(capacity);
    let (device_end, mut device_side) = tokio::io::duplex(capacity);
    let (stall, mut stalled) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let (mut from_host, mut from_device) = (vec![0u8; 16 * 1024], vec![0u8; 16 * 1024]);
        loop {
            tokio::select! {
                _ = stalled.changed() => {
                    // Hold both sides open and move nothing more.
                    std::future::pending::<()>().await;
                }
                read = host_side.read(&mut from_host) => match read {
                    Ok(n) if n > 0 => {
                        let _ = device_side.write_all(&from_host[..n]).await;
                        tokio::time::sleep(chunk_delay).await;
                    }
                    _ => break,
                },
                read = device_side.read(&mut from_device) => match read {
                    Ok(n) if n > 0 => {
                        let _ = host_side.write_all(&from_device[..n]).await;
                        tokio::time::sleep(chunk_delay).await;
                    }
                    _ => break,
                },
            }
        }
    });
    (host_end, device_end, stall)
}

async fn channels_over(
    host_end: tokio::io::DuplexStream,
    device_end: tokio::io::DuplexStream,
    capabilities: Vec<String>,
) -> (
    Arc<streamcore_transport::channel::SecureListener>,
    Arc<streamcore_transport::channel::SecureChannel>,
    Arc<streamcore_transport::channel::SecureChannel>,
) {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let options = SessionOptions {
        required: capabilities.clone(),
        capabilities,
    };
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        options.clone(),
    )
    .await
    .unwrap();
    listener
        .accept_stream(UsbPipe {
            io: Box::new(host_end),
            medium: Medium::UsbAdb,
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
            medium: Medium::UsbAdb,
            device: "host".into(),
        },
        options,
        OperationCancellation::new(),
    )
    .await
    .unwrap();
    (listener, server.await.unwrap(), channel)
}

/// Reads on a task until the channel reports an error; the task returns when
/// that happened. (Cancelling a read in progress closes a channel, so the tests
/// never time out the read itself.)
fn watch_for_close(
    channel: &Arc<streamcore_transport::channel::SecureChannel>,
) -> tokio::task::JoinHandle<std::time::Instant> {
    let channel = channel.clone();
    tokio::spawn(async move {
        while channel.receive().await.is_ok() {}
        std::time::Instant::now()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pipe_that_goes_half_open_is_dead_within_a_second_on_both_sides() {
    let (host_end, device_end, stall) = stalling_relay(64 * 1024, Duration::ZERO);
    let (_listener, server, channel) =
        channels_over(host_end, device_end, vec![CLOCK_CAPABILITY.into()]).await;
    let (device_closed, host_closed) = (watch_for_close(&channel), watch_for_close(&server));

    // An idle but healthy pipe stays up: the pings keep it alive.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!device_closed.is_finished() && !host_closed.is_finished());
    channel.send(b"still here".to_vec()).await.unwrap();

    // The cable goes: no end-of-file, no error, the bytes just stop.
    let stalled_at = std::time::Instant::now();
    stall.send(true).unwrap();
    for (side, closed) in [("device", device_closed), ("host", host_closed)] {
        let at = tokio::time::timeout(Duration::from_secs(3), closed)
            .await
            .unwrap_or_else(|_| panic!("the {side} never noticed the dead pipe"))
            .unwrap();
        let took = at.duration_since(stalled_at);
        assert!(
            took >= Duration::from_millis(500) && took <= Duration::from_millis(2_500),
            "the {side} took {took:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_typed_pings_a_pipe_has_no_liveness_of_its_own() {
    let (host_end, device_end, stall) = stalling_relay(64 * 1024, Duration::ZERO);
    let (_listener, _server, channel) = channels_over(host_end, device_end, vec![]).await;
    let closed = watch_for_close(&channel);
    stall.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!closed.is_finished(), "the channel closed without clock.v1");
}

/// Reads on a task, counting bulk messages, until the channel reports an error.
fn count_bulk(
    channel: &Arc<streamcore_transport::channel::SecureChannel>,
) -> (
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let (channel, count) = (
        channel.clone(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    let counted = count.clone();
    let task = tokio::spawn(async move {
        while let Ok(message) = channel.receive().await {
            if message.first() == Some(&0x40) {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    (count, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_pipe_is_not_mistaken_for_a_dead_one() {
    use streamcore_media::binary::Frame;
    // A relay with megabytes of buffer on each side that moves about 4 MB/s: a
    // pong queues behind the transfer for longer than the silent bound, while
    // bytes keep arriving the whole time.
    const MESSAGES: usize = 40;
    let (host_end, device_end, _stall) = stalling_relay(4 * 1024 * 1024, Duration::from_millis(4));
    let (_listener, server, channel) =
        channels_over(host_end, device_end, vec![CLOCK_CAPABILITY.into()]).await;
    let (received, reading) = count_bulk(&channel);
    let host_closed = watch_for_close(&server);
    let payload = vec![0x5a; 256 * 1024];
    for _ in 0..MESSAGES {
        let frame = Frame {
            kind: 0x40,
            payload: &payload,
        }
        .encode()
        .unwrap();
        // The bulk lane's queue is bounded: wait for room.
        loop {
            match server.enqueue_application_frame(frame.clone()) {
                Ok(_) => break,
                Err(streamcore_transport::channel::ChannelError::Backpressure) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("the busy pipe was closed: {error:?}"),
            }
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while received.load(std::sync::atomic::Ordering::Relaxed) < MESSAGES {
        assert!(
            !reading.is_finished(),
            "the device side closed mid-transfer"
        );
        assert!(
            !host_closed.is_finished(),
            "the host side closed mid-transfer"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the transfer never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // And it is still a working channel afterwards.
    let probe = server.probe(3, 20, 2_000).await.unwrap();
    assert_eq!(probe.answered, 3);
}
