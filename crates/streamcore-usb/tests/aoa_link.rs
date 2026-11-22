//! An accessory connection end to end through the binding surface: two
//! platform byte streams joined like a cable, a link on each, the host
//! opening a pipe and the device dialling the host over it with the pinned
//! handshake.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use streamcore_transport::{
    OperationCancellation,
    channel::{ChannelLocator, SessionOptions, connect_secure_pipe, listen_secure},
    duplex::{ByteDuplex, DuplexError},
    identity::PeerIdentity,
    transport::{Medium, PipeOutcome},
};
use streamcore_usb::{
    UsbService,
    aoa::LinkRole,
    ffi::{UsbAccessoryLink, UsbLinkError},
};

/// One direction of the cable.
#[derive(Default)]
struct Wire {
    bytes: Mutex<VecDeque<u8>>,
    wake: Condvar,
    cut: AtomicBool,
}

impl Wire {
    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        let _bytes = self.bytes.lock().unwrap();
        self.wake.notify_all();
    }
}

/// One end of the cable, as a platform's blocking stream.
struct End {
    incoming: Arc<Wire>,
    outgoing: Arc<Wire>,
}

impl ByteDuplex for End {
    fn read(&self, max: u32) -> Result<Vec<u8>, DuplexError> {
        let mut bytes = self.incoming.bytes.lock().unwrap();
        loop {
            if self.incoming.cut.load(Ordering::SeqCst) {
                return Err(DuplexError::Closed);
            }
            if !bytes.is_empty() {
                let take = bytes.len().min(max as usize);
                return Ok(bytes.drain(..take).collect());
            }
            bytes = self.incoming.wake.wait(bytes).unwrap();
        }
    }
    fn write(&self, data: Vec<u8>) -> Result<(), DuplexError> {
        if self.outgoing.cut.load(Ordering::SeqCst) {
            return Err(DuplexError::Closed);
        }
        self.outgoing.bytes.lock().unwrap().extend(data);
        self.outgoing.wake.notify_all();
        Ok(())
    }
    fn shutdown(&self) {
        // Unplugged at this end: both directions stop.
        self.incoming.cut();
        self.outgoing.cut();
    }
}

fn cable() -> (Arc<End>, Arc<End>) {
    let (a, b) = (Arc::new(Wire::default()), Arc::new(Wire::default()));
    (
        Arc::new(End {
            incoming: a.clone(),
            outgoing: b.clone(),
        }),
        Arc::new(End {
            incoming: b,
            outgoing: a,
        }),
    )
}

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec![],
        required: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_dials_the_host_over_an_accessory_pipe_and_an_unplug_ends_it() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let (host_end, device_end) = cable();
    let host_link = UsbAccessoryLink::start(host_end.clone(), LinkRole::Host, "tablet-1".into());
    let device_link = UsbAccessoryLink::start(device_end, LinkRole::Device, "host".into());

    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        options(),
    )
    .await
    .unwrap();
    let pipe = host_link.open_pipe(UsbService::Session).await.unwrap();
    let admission = listener.accept_pipe(pipe.clone()).unwrap();
    assert!(listener.accept_pipe(pipe).is_err(), "a handle is used once");
    let accepting = listener.clone();
    let server = tokio::spawn(async move { accepting.accept().await.unwrap() });

    let accepted = device_link.accept().await.unwrap();
    assert_eq!(accepted.service, UsbService::Session);
    let channel = connect_secure_pipe(
        client,
        host.certificate(),
        accepted.pipe,
        options(),
        OperationCancellation::new(),
    )
    .await
    .unwrap();
    let server = server.await.unwrap();
    assert_eq!(admission.outcome().await, PipeOutcome::Accepted);
    assert_eq!(
        server.locator(),
        ChannelLocator::Usb {
            medium: Medium::UsbAoa,
            device: "tablet-1".into()
        }
    );

    let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    channel.send(big.clone()).await.unwrap();
    assert!(server.receive().await.unwrap() == big);
    server.send(b"from the host".to_vec()).await.unwrap();
    assert_eq!(channel.receive().await.unwrap(), b"from the host");

    // The cable is pulled: both links end, and so do the channels on them.
    host_end.shutdown();
    tokio::time::timeout(Duration::from_secs(5), async {
        host_link.closed().await;
        device_link.closed().await;
    })
    .await
    .expect("the links ended");
    assert!(matches!(
        host_link.open_pipe(UsbService::Session).await,
        Err(UsbLinkError::Closed)
    ));
    assert!(device_link.accept().await.is_none());
    let ended = tokio::time::timeout(Duration::from_secs(10), channel.receive()).await;
    assert!(ended.expect("the channel noticed").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_host_end_opens_pipes() {
    let (host_end, device_end) = cable();
    let _host = UsbAccessoryLink::start(host_end, LinkRole::Host, "tablet-1".into());
    let device = UsbAccessoryLink::start(device_end, LinkRole::Device, "host".into());
    assert!(matches!(
        device.open_pipe(UsbService::Pairing).await,
        Err(UsbLinkError::Role)
    ));
}

/// A descriptor's read cannot be woken from another thread. Ending the link
/// must still release the descriptor while the cable stays in: the peer's
/// answer to the parting hello is what the blocked read returns with.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_a_link_on_a_descriptor_releases_it_without_an_unplug() {
    use std::os::fd::IntoRawFd;
    use streamcore_usb::aoa::AccessoryLink;
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let peer = AccessoryLink::start(
        Box::new(tokio::net::UnixStream::from_std(theirs).unwrap()),
        LinkRole::Host,
    );
    let device =
        UsbAccessoryLink::start_descriptor(ours.into_raw_fd(), LinkRole::Device, "host".into())
            .unwrap();
    // Both ends up: a pipe opens.
    let pipe = peer.open(UsbService::Session).await.unwrap();
    assert!(device.accept().await.is_some());
    drop(pipe);

    device.end();
    // The peer sees the stream end only once the descriptor is closed, which
    // needs the reading thread to return from its read.
    tokio::time::timeout(Duration::from_secs(5), peer.closed())
        .await
        .expect("the descriptor was released");
}

/// Pairing with no network: the host opens a pairing pipe on the accessory
/// link while its window is open, the device pairs over it with the code,
/// and the certificates they exchanged then carry a session on a second pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_pairs_over_an_accessory_pipe_and_then_connects() {
    use streamcore_transport::identity::certificate_fingerprint;
    use streamcore_transport::pairing_transport::{listen_pairing, pair_with_host_pipe};
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let (host_end, device_end) = cable();
    let host_link = UsbAccessoryLink::start(host_end, LinkRole::Host, "tablet-1".into());
    let device_link = UsbAccessoryLink::start(device_end, LinkRole::Device, "host".into());

    // The host's user opened a pairing window; its code is typed on the device.
    let window = listen_pairing("127.0.0.1:0".into(), host.clone())
        .await
        .unwrap();
    let code = window.display_code().unwrap();
    let pipe = host_link.open_pipe(UsbService::Pairing).await.unwrap();
    let admission = window.accept_pipe(pipe).unwrap();

    let accepted = device_link.accept().await.unwrap();
    assert_eq!(accepted.service, UsbService::Pairing);
    let pairing = pair_with_host_pipe(
        accepted.pipe,
        client.clone(),
        code,
        Some(certificate_fingerprint(&host.certificate())),
        OperationCancellation::new(),
    );
    let persisting = async {
        let pending = window.next_verified().await.unwrap();
        let certificate = pending.peer_certificate();
        pending.confirm_persisted().await.unwrap();
        certificate
    };
    let (host_certificate, client_certificate) = tokio::join!(pairing, persisting);
    let host_certificate = host_certificate.unwrap();
    assert_eq!(host_certificate, host.certificate());
    assert_eq!(client_certificate, client.certificate());
    assert_eq!(admission.outcome().await, PipeOutcome::Accepted);

    // What pairing exchanged is all the session needs.
    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host,
        vec![client_certificate],
        options(),
    )
    .await
    .unwrap();
    let pipe = host_link.open_pipe(UsbService::Session).await.unwrap();
    listener.accept_pipe(pipe).unwrap();
    let accepting = listener.clone();
    let server = tokio::spawn(async move { accepting.accept().await.unwrap() });
    let accepted = device_link.accept().await.unwrap();
    let channel = connect_secure_pipe(
        client,
        host_certificate,
        accepted.pipe,
        options(),
        OperationCancellation::new(),
    )
    .await
    .unwrap();
    let server = server.await.unwrap();
    channel
        .send(b"paired over the cable".to_vec())
        .await
        .unwrap();
    assert_eq!(server.receive().await.unwrap(), b"paired over the cable");

    // With the window closed, a pairing pipe is refused.
    window.close_and_wait().await.unwrap();
    let pipe = host_link.open_pipe(UsbService::Pairing).await.unwrap();
    assert!(window.accept_pipe(pipe).is_err());
}
