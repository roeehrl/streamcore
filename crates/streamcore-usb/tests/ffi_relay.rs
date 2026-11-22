//! Relayed pipes through the binding surface: a host keeps a pipe to a
//! "cabled" device whose listener is a loopback port, and the device dials
//! the host over the pipe it accepts.

use std::sync::Arc;
use streamcore_transport::{
    OperationCancellation,
    channel::{ChannelLocator, SessionOptions, connect_secure_pipe, listen_secure},
    identity::PeerIdentity,
    transport::{Medium, PipeIo},
};
use streamcore_usb::{
    UsbDevice, UsbError, UsbEvent, UsbPipeProvider, UsbService,
    ffi::{UsbDeviceListener, UsbHostPipes, UsbProviderError},
};
use tokio::{net::TcpStream, sync::Mutex};

/// A provider with one device attached, whose pipes are connections to a
/// loopback port (as a relay's are, from the host's side).
struct OneDevice {
    port: u16,
    events: Mutex<Vec<UsbEvent>>,
}

fn device() -> UsbDevice {
    UsbDevice {
        medium: Medium::UsbMux,
        label: "pad-1".into(),
    }
}

#[async_trait::async_trait]
impl UsbPipeProvider for OneDevice {
    async fn next_event(&self) -> Option<UsbEvent> {
        match self.events.lock().await.pop() {
            Some(event) => Some(event),
            // Attached for good.
            None => std::future::pending().await,
        }
    }
    async fn open(&self, _device: &UsbDevice, _service: UsbService) -> Result<PipeIo, UsbError> {
        Ok(Box::new(
            TcpStream::connect(("127.0.0.1", self.port)).await?,
        ))
    }
}

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec![],
        required: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_keeps_a_pipe_and_the_device_dials_over_it() {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let client = Arc::new(PeerIdentity::generate().unwrap());
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let listener_on_device = UsbDeviceListener::bind(vec![port], Medium::UsbMux)
        .await
        .unwrap();
    assert_eq!(listener_on_device.port(), port);

    let listener = listen_secure(
        "127.0.0.1:0".into(),
        host.clone(),
        vec![client.certificate()],
        options(),
    )
    .await
    .unwrap();
    let provider = Arc::new(OneDevice {
        port,
        events: Mutex::new(vec![UsbEvent::Attached(device())]),
    });
    let kept = UsbHostPipes::keep(provider, listener.clone()).unwrap();
    let accepting = listener.clone();
    let server = tokio::spawn(async move { accepting.accept().await.unwrap() });

    let pipe = listener_on_device.accept().await.unwrap();
    let channel = connect_secure_pipe(
        client,
        host.certificate(),
        pipe,
        options(),
        OperationCancellation::new(),
    )
    .await
    .unwrap();
    let server = server.await.unwrap();
    assert_eq!(
        server.locator(),
        ChannelLocator::Usb {
            medium: Medium::UsbMux,
            device: "pad-1".into()
        }
    );
    channel.send(b"over the cable".to_vec()).await.unwrap();
    assert_eq!(server.receive().await.unwrap(), b"over the cable");
    kept.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_ports_are_refused() {
    assert!(matches!(
        UsbDeviceListener::bind(vec![], Medium::UsbMux).await,
        Err(UsbProviderError::Invalid)
    ));
    assert!(matches!(
        UsbDeviceListener::bind(vec![4000], Medium::UsbNet).await,
        Err(UsbProviderError::Invalid)
    ));
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let listener = listen_secure("127.0.0.1:0".into(), host, vec![], options())
        .await
        .unwrap();
    assert!(matches!(
        UsbHostPipes::usbmux(listener.clone(), "probe".into(), 0),
        Err(UsbProviderError::Invalid)
    ));
    assert!(matches!(
        UsbHostPipes::usbmux(listener.clone(), "not a plain name!".into(), 4000),
        Err(UsbProviderError::Invalid)
    ));
    assert!(matches!(
        UsbHostPipes::adb(listener, 0),
        Err(UsbProviderError::Invalid)
    ));
}
