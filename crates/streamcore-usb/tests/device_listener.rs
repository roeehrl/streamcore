//! The device-side listener end to end over loopback: the "host" connects to
//! the device's port (as a provider's pipe does), hands its end to a listener,
//! and the device dials over what it accepted.

use std::{sync::Arc, time::Duration};
use streamcore_transport::{
    OperationCancellation,
    channel::{SessionOptions, UsbPipe, connect_secure_stream_with, listen_secure},
    identity::PeerIdentity,
    multiplex::binary_lanes,
    transport::Medium,
};
use streamcore_usb::{
    UsbError,
    device::{DeviceListener, MAX_DEVICE_PIPES},
};
use tokio::net::{TcpListener, TcpStream};

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec![],
        required: vec![],
    }
}

async fn free_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    probe.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_device_dials_the_host_over_a_pipe_it_accepted() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let port = free_port().await;
    let device = DeviceListener::bind(&[port], Medium::UsbAdb).await.unwrap();
    assert_eq!(device.port().unwrap(), port);

    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        options(),
    )
    .await
    .unwrap();
    // The provider's pipe: a connection to the device's port.
    let pipe = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let handle = listener
        .accept_stream(UsbPipe {
            io: Box::new(pipe),
            medium: Medium::UsbAdb,
            device: "device-1".into(),
        })
        .unwrap();
    let accepting = listener.clone();
    let server = tokio::spawn(async move { accepting.accept().await.unwrap() });

    let accepted = device.accept().await.unwrap();
    assert_eq!(accepted.medium, Medium::UsbAdb);
    let channel = connect_secure_stream_with(
        binary_lanes,
        client,
        host.certificate(),
        accepted,
        options(),
        OperationCancellation::new(),
    )
    .await
    .unwrap();
    let server = server.await.unwrap();
    assert_eq!(
        handle.outcome().await,
        streamcore_transport::transport::PipeOutcome::Accepted
    );
    channel.send(b"from the device".to_vec()).await.unwrap();
    assert_eq!(server.receive().await.unwrap(), b"from the device");
}

#[tokio::test]
async fn a_taken_port_moves_the_listener_to_the_next_and_bad_lists_are_refused() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken_port = taken.local_addr().unwrap().port();
    let next = free_port().await;
    let device = DeviceListener::bind(&[taken_port, next], Medium::UsbMux)
        .await
        .unwrap();
    assert_eq!(device.port().unwrap(), next);
    // Every port taken: the last bind error.
    assert!(matches!(
        DeviceListener::bind(&[taken_port], Medium::UsbMux).await,
        Err(UsbError::Io(_))
    ));
    for (ports, medium) in [
        (&[][..], Medium::UsbAdb),
        (&[0][..], Medium::UsbAdb),
        // A USB network interface is an IP link, not a relayed pipe.
        (&[next][..], Medium::UsbNet),
    ] {
        assert!(matches!(
            DeviceListener::bind(ports, medium).await,
            Err(UsbError::Protocol)
        ));
    }
}

#[tokio::test]
async fn a_local_app_cannot_use_up_the_listener() {
    let port = free_port().await;
    let device = DeviceListener::bind(&[port], Medium::UsbAdb).await.unwrap();
    // More connections than places, as any app on the device could open.
    let mut strangers = Vec::new();
    for _ in 0..MAX_DEVICE_PIPES + 2 {
        strangers.push(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
    }
    let mut held = Vec::new();
    for _ in 0..MAX_DEVICE_PIPES {
        held.push(device.accept().await.unwrap());
    }
    // Every place is held: the next accept waits.
    let waiting = tokio::time::timeout(Duration::from_millis(200), device.accept()).await;
    assert!(waiting.is_err(), "a fifth pipe was accepted");
    // Dropping one frees its place.
    held.pop();
    let freed = tokio::time::timeout(Duration::from_secs(2), device.accept()).await;
    assert!(freed.is_ok_and(|pipe| pipe.is_ok()));
}
