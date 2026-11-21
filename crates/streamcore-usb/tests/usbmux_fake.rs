//! The multiplexer provider against a fake service on loopback TCP: attach and
//! detach, a pipe to a device port, refusals, and a service that lies.

use std::{net::SocketAddr, time::Duration};
use streamcore_transport::transport::Medium;
use streamcore_usb::{
    UsbDevice, UsbError, UsbEvent, UsbPipeProvider, UsbService,
    usbmux::{MuxAddress, MuxPorts, MuxProvider, MuxService},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

fn message(body: &str) -> Vec<u8> {
    let payload = format!("<?xml version=\"1.0\"?><plist version=\"1.0\">{body}</plist>");
    let mut out = Vec::new();
    for word in [16 + payload.len() as u32, 1, 8, 0] {
        out.extend(word.to_le_bytes());
    }
    out.extend(payload.as_bytes());
    out
}

fn result(number: i64) -> Vec<u8> {
    message(&format!(
        "<dict><key>MessageType</key><string>Result</string><key>Number</key><integer>{number}</integer></dict>"
    ))
}

fn attached(id: u32, serial: &str, connection: &str) -> Vec<u8> {
    message(&format!(
        "<dict><key>MessageType</key><string>Attached</string><key>DeviceID</key><integer>{id}</integer>\
         <key>Properties</key><dict><key>ConnectionType</key><string>{connection}</string>\
         <key>SerialNumber</key><string>{serial}</string><key>DeviceID</key><integer>{id}</integer></dict></dict>"
    ))
}

fn detached(id: u32) -> Vec<u8> {
    message(&format!(
        "<dict><key>MessageType</key><string>Detached</string><key>DeviceID</key><integer>{id}</integer></dict>"
    ))
}

async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await.ok()?;
    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let mut payload = vec![0; length.checked_sub(16)?];
    stream.read_exact(&mut payload).await.ok()?;
    String::from_utf8(payload).ok()
}

fn field(request: &str, key: &str) -> Option<i64> {
    let after = request.split(&format!("<key>{key}</key>")).nth(1)?;
    after
        .split("<integer>")
        .nth(1)?
        .split("</integer>")
        .next()?
        .parse()
        .ok()
}

/// A fake service: `Listen` connections get the messages sent on `events`;
/// `Connect` to device 7, port 47820 is joined to `device`.
async fn fake_service(device: SocketAddr, mut events: mpsc::Receiver<Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut listening: Option<TcpStream> = None;
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { listening = None; continue };
                    if let Some(stream) = listening.as_mut() { let _ = stream.write_all(&event).await; }
                }
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { break };
                    let Some(request) = read_request(&mut stream).await else { continue };
                    if request.contains("<string>Listen</string>") {
                        let _ = stream.write_all(&result(0)).await;
                        listening = Some(stream);
                    } else if request.contains("<string>Connect</string>") {
                        let port = field(&request, "PortNumber").map(|p| (p as u16).swap_bytes());
                        match (field(&request, "DeviceID"), port) {
                            (Some(7), Some(47820)) => {
                                let _ = stream.write_all(&result(0)).await;
                                tokio::spawn(async move {
                                    if let Ok(mut device) = TcpStream::connect(device).await {
                                        let _ = tokio::io::copy_bidirectional(&mut stream, &mut device).await;
                                    }
                                });
                            }
                            (Some(7), _) => { let _ = stream.write_all(&result(3)).await; }
                            _ => { let _ = stream.write_all(&result(2)).await; }
                        }
                    }
                }
            }
        }
    });
    address
}

fn provider(address: SocketAddr) -> MuxProvider {
    MuxProvider::new(
        MuxService::new(MuxAddress::Tcp(address), "test-client").unwrap(),
        MuxPorts {
            session: 47820,
            pairing: 47821,
        },
    )
}

fn device(label: &str) -> UsbDevice {
    UsbDevice {
        medium: Medium::UsbMux,
        label: label.into(),
    }
}

#[tokio::test]
async fn attach_a_pipe_refusals_and_detach() {
    let device_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (events, receiver) = mpsc::channel(8);
    let address = fake_service(device_listener.local_addr().unwrap(), receiver).await;
    let provider = provider(address);

    // Not attached yet: no pipe.
    assert!(matches!(
        provider
            .open(&device("DEVICE-0001"), UsbService::Session)
            .await,
        Err(UsbError::Refused(_))
    ));

    // A device the service reaches over the network is not a cable.
    let waiting = tokio::spawn(async move {
        let event = provider.next_event().await;
        (provider, event)
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    events
        .send(attached(9, "DEVICE-0002", "Network"))
        .await
        .unwrap();
    events
        .send(attached(7, "DEVICE-0001", "USB"))
        .await
        .unwrap();
    let (provider, event) = waiting.await.unwrap();
    assert_eq!(event, Some(UsbEvent::Attached(device("DEVICE-0001"))));

    // The pipe reaches the device's listener and carries bytes both ways.
    let on_device = tokio::spawn(async move {
        let (mut stream, _) = device_listener.accept().await.unwrap();
        let mut line = [0u8; 5];
        stream.read_exact(&mut line).await.unwrap();
        stream.write_all(b"pong!").await.unwrap();
        line
    });
    let mut pipe = provider
        .open(&device("DEVICE-0001"), UsbService::Session)
        .await
        .unwrap();
    pipe.write_all(b"ping!").await.unwrap();
    let mut reply = [0u8; 5];
    pipe.read_exact(&mut reply).await.unwrap();
    assert_eq!((&on_device.await.unwrap(), &reply), (b"ping!", b"pong!"));

    // The pairing port is closed on this device.
    assert!(matches!(
        provider.open(&device("DEVICE-0001"), UsbService::Pairing).await,
        Err(UsbError::Refused(reason)) if reason == "closed"
    ));
    // Another medium's device is not this provider's.
    let other = UsbDevice {
        medium: Medium::UsbAdb,
        label: "DEVICE-0001".into(),
    };
    assert!(matches!(
        provider.open(&other, UsbService::Session).await,
        Err(UsbError::Protocol)
    ));

    // Unplugged: detached, and no more pipes. Then the service stops: the end.
    events.send(detached(7)).await.unwrap();
    assert_eq!(
        provider.next_event().await,
        Some(UsbEvent::Detached(device("DEVICE-0001")))
    );
    assert!(matches!(
        provider
            .open(&device("DEVICE-0001"), UsbService::Session)
            .await,
        Err(UsbError::Refused(_))
    ));
    drop(events);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), provider.next_event())
            .await
            .unwrap(),
        None
    );
}

/// A service that answers each connection with `reply` and closes.
async fn lying_service(reply: Vec<u8>) -> MuxService {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = read_request(&mut stream).await;
            let _ = stream.write_all(&reply).await;
        }
    });
    MuxService::new(MuxAddress::Tcp(address), "test-client").unwrap()
}

#[tokio::test]
async fn a_service_that_breaks_the_protocol_is_an_error_not_a_hang_or_a_large_allocation() {
    // A length far over the bound: refused before anything is allocated for it.
    let mut huge = Vec::new();
    for word in [u32::MAX, 1, 8, 0] {
        huge.extend(word.to_le_bytes());
    }
    assert!(matches!(
        lying_service(huge).await.devices().await,
        Err(UsbError::Protocol)
    ));
    // A length shorter than its own header.
    let mut short = Vec::new();
    for word in [4u32, 1, 8, 0] {
        short.extend(word.to_le_bytes());
    }
    assert!(matches!(
        lying_service(short).await.devices().await,
        Err(UsbError::Protocol)
    ));
    // Not a property list, and a list without the expected key.
    assert!(matches!(
        lying_service(message("garbage")).await.devices().await,
        Err(UsbError::Protocol)
    ));
    assert!(matches!(
        lying_service(message("<dict/>")).await.devices().await,
        Err(UsbError::Protocol)
    ));
    // A Connect answered with something that is not a Result.
    assert!(matches!(
        lying_service(attached(1, "X", "USB"))
            .await
            .open(1, 47820)
            .await,
        Err(UsbError::Protocol)
    ));
    // Silent: the deadline ends it. Nothing listening: the service is missing.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let service =
        MuxService::new(MuxAddress::Tcp(silent.local_addr().unwrap()), "test-client").unwrap();
    assert!(matches!(service.devices().await, Err(UsbError::Timeout)));
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let service = MuxService::new(MuxAddress::Tcp(address), "test-client").unwrap();
    assert!(matches!(
        service.devices().await,
        Err(UsbError::ServiceMissing)
    ));
    // Only a loopback address, and only a plain client name.
    assert!(MuxService::new(MuxAddress::Tcp("192.0.2.1:27015".parse().unwrap()), "x").is_err());
    assert!(MuxService::new(MuxAddress::platform_default(), "a<b").is_err());
}
