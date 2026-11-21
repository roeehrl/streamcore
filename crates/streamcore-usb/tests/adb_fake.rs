//! The adb provider against a fake adb server on loopback: device tracking, a
//! pipe that carries a pinned session end to end, and a server that lies. A
//! fake server is also the threat model: anything on this machine can listen
//! on the adb port, so nothing it says may be trusted.

use std::{net::SocketAddr, sync::Arc, time::Duration};
use streamcore_transport::{
    identity::PeerIdentity,
    protocol::{Hello, PROTOCOL_MAJOR},
    transport::{Medium, PeerLocator, SessionListener, dial_pipe},
};
use streamcore_usb::{
    UsbDevice, UsbError, UsbEvent, UsbPipeProvider, UsbService,
    adb::{AdbEndpoints, AdbProvider, AdbServer, AdbTarget},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

fn hello() -> Hello {
    Hello {
        major: PROTOCOL_MAJOR,
        capabilities: vec![],
        required: vec![],
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.ok()?;
    let length = usize::from_str_radix(std::str::from_utf8(&length).ok()?, 16).ok()?;
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.ok()?;
    String::from_utf8(payload).ok()
}

async fn text(stream: &mut TcpStream, body: &str) {
    let _ = stream
        .write_all(format!("{:04x}{body}", body.len()).as_bytes())
        .await;
}

/// A fake adb server: `host:version`, `host:track-devices-l` (listings from
/// `listings`), and `host:transport:<serial>` + `tcp:<port>`, which it joins
/// to `device` (the "device's" loopback listener) for the serial `SERIAL`.
const SERIAL: &str = "FAKE0001";
async fn fake_server(device: SocketAddr, mut listings: mpsc::Receiver<String>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut tracker: Option<TcpStream> = None;
        loop {
            tokio::select! {
                listing = listings.recv() => {
                    let Some(listing) = listing else { tracker = None; continue };
                    if let Some(stream) = tracker.as_mut() { text(stream, &listing).await; }
                }
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { break };
                    match read_request(&mut stream).await.as_deref() {
                        Some("host:version") => {
                            let _ = stream.write_all(b"OKAY").await;
                            text(&mut stream, "0029").await;
                        }
                        Some("host:track-devices-l") => {
                            let _ = stream.write_all(b"OKAY").await;
                            tracker = Some(stream);
                        }
                        Some(request) if request == format!("host:transport:{SERIAL}") => {
                            let _ = stream.write_all(b"OKAY").await;
                            if read_request(&mut stream).await.as_deref() != Some("tcp:47820") {
                                let _ = stream.write_all(b"FAIL").await;
                                text(&mut stream, "closed").await;
                                continue;
                            }
                            let _ = stream.write_all(b"OKAY").await;
                            tokio::spawn(async move {
                                if let Ok(mut device) = TcpStream::connect(device).await {
                                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut device).await;
                                }
                            });
                        }
                        _ => {
                            let _ = stream.write_all(b"FAIL").await;
                            text(&mut stream, "device not found").await;
                        }
                    }
                }
            }
        }
    });
    address
}

fn provider(server: SocketAddr) -> AdbProvider {
    AdbProvider::new(
        AdbServer::new(server).unwrap(),
        AdbEndpoints {
            session: AdbTarget::Tcp(47820),
            pairing: AdbTarget::Tcp(47821),
        },
    )
}

fn device(label: &str) -> UsbDevice {
    UsbDevice {
        medium: Medium::UsbAdb,
        label: label.into(),
    }
}

#[tokio::test]
async fn tracking_reports_attach_and_detach_and_a_pipe_carries_a_pinned_session() {
    let host = PeerIdentity::generate().unwrap();
    let client = Arc::new(PeerIdentity::generate().unwrap());
    // The "device": a loopback listener, dialing the host's pins on what it accepts.
    let device_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device_address = device_listener.local_addr().unwrap();
    let (listings, receiver) = mpsc::channel(4);
    let server = fake_server(device_address, receiver).await;
    let provider = provider(server);
    assert_eq!(
        AdbServer::new(server).unwrap().version().await.unwrap(),
        0x29
    );

    // A wireless entry and an unauthorized device are not cabled, ready devices.
    let events = tokio::spawn(async move {
        let first = provider.next_event().await;
        (provider, first)
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    listings
        .send(format!(
            "{SERIAL} device usb:1-1 transport_id:1\n192.0.2.9:5555 device transport_id:2\nOTHER unauthorized usb:1-2\n"
        ))
        .await
        .unwrap();
    let (provider, first) = events.await.unwrap();
    assert_eq!(first, Some(UsbEvent::Attached(device(SERIAL))));

    // The host opens a pipe and hands it to its listener; the device dials on it.
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let pipe = provider
        .open(&device(SERIAL), UsbService::Session)
        .await
        .unwrap();
    listener
        .accept_stream(
            pipe,
            PeerLocator::Usb {
                medium: Medium::UsbAdb,
                device: SERIAL.into(),
            },
        )
        .unwrap();
    let host_certificate = host.certificate();
    let dialing = client.clone();
    let on_device = tokio::spawn(async move {
        let (stream, _) = device_listener.accept().await.unwrap();
        let locator = PeerLocator::Usb {
            medium: Medium::UsbAdb,
            device: "host".into(),
        };
        let mut session = dial_pipe(
            &dialing,
            host_certificate,
            Box::new(stream),
            locator,
            hello(),
        )
        .await
        .unwrap();
        session.send(b"through adb").await.unwrap();
        session.receive().await.unwrap()
    });
    let mut session = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.peer_fingerprint, client.fingerprint());
    assert_eq!(session.receive().await.unwrap(), b"through adb");
    session.send(b"acknowledged").await.unwrap();
    assert_eq!(on_device.await.unwrap(), b"acknowledged");

    // The pairing port is closed on this device: refused, with the reason.
    assert!(matches!(
        provider.open(&device(SERIAL), UsbService::Pairing).await,
        Err(UsbError::Refused(reason)) if reason == "closed"
    ));

    // Unplugged, then the server stops: detached, then the end.
    listings.send(String::new()).await.unwrap();
    assert_eq!(
        provider.next_event().await,
        Some(UsbEvent::Detached(device(SERIAL)))
    );
    drop(listings);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), provider.next_event())
            .await
            .unwrap(),
        None
    );
}

/// A server that answers each connection with `reply` and closes.
async fn lying_server(reply: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = read_request(&mut stream).await;
            let _ = stream.write_all(reply).await;
        }
    });
    address
}

#[tokio::test]
async fn a_server_that_breaks_the_protocol_is_an_error_not_a_hang_or_a_large_allocation() {
    // Not OKAY or FAIL.
    let server = AdbServer::new(lying_server(b"HTTP/1.1 200 OK\r\n").await).unwrap();
    assert!(matches!(server.version().await, Err(UsbError::Protocol)));
    // A length that is not hex.
    let server = AdbServer::new(lying_server(b"OKAYzzzz").await).unwrap();
    assert!(matches!(server.version().await, Err(UsbError::Protocol)));
    // A listing longer than any real one: refused before it is read.
    let server = AdbServer::new(lying_server(b"OKAYffff").await).unwrap();
    assert!(matches!(server.devices().await, Err(UsbError::Protocol)));
    // Accepts and says nothing: the exchange's deadline ends it.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = AdbServer::new(silent.local_addr().unwrap()).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(server.version().await, Err(UsbError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(4));
    // Nothing listening.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let server = AdbServer::new(address).unwrap();
    assert!(matches!(
        server.version().await,
        Err(UsbError::ServiceMissing)
    ));
}

#[tokio::test]
async fn a_fake_server_cannot_impersonate_the_device() {
    // The fake server joins the pipe to an impostor instead of the paired
    // device: the pinned handshake fails, and no session reaches the host.
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let impostor = PeerIdentity::generate().unwrap();
    let device_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_listings, receiver) = mpsc::channel(1);
    let server = fake_server(device_listener.local_addr().unwrap(), receiver).await;
    let provider = provider(server);
    let listener = SessionListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        &host,
        vec![client.certificate()],
        hello(),
    )
    .await
    .unwrap();
    let pipe = provider
        .open(&device(SERIAL), UsbService::Session)
        .await
        .unwrap();
    let locator = PeerLocator::Usb {
        medium: Medium::UsbAdb,
        device: SERIAL.into(),
    };
    listener.accept_stream(pipe, locator.clone()).unwrap();
    let host_certificate = host.certificate();
    tokio::spawn(async move {
        let (stream, _) = device_listener.accept().await.unwrap();
        let _ = dial_pipe(
            &impostor,
            host_certificate,
            Box::new(stream),
            locator,
            hello(),
        )
        .await;
    });
    let accepted = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap();
    assert!(accepted.is_err());
}
