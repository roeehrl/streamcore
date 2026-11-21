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
    let pipe = host_link.open(UsbService::Session).await.unwrap();
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
        host_link.open(UsbService::Session).await,
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
        device.open(UsbService::Pairing).await,
        Err(UsbLinkError::Role)
    ));
}
