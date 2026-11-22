//! The binding surface (feature `ffi`): a native host or device app supplies
//! the accessory's raw byte stream and gets pipes for the secure transport.

use crate::{
    UsbError, UsbService,
    aoa::{AccessoryLink, LinkRole},
};
use std::sync::Arc;
use streamcore_transport::{
    channel::UsbPipe,
    duplex::{ByteDuplex, UsbPipeHandle, pipe_from_duplex},
    transport::Medium,
};
use tokio::sync::oneshot;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UsbLinkError {
    /// The link ended (the cable, a framing error, or `end`).
    #[error("the link is closed")]
    Closed,
    /// The device refused the pipe: it holds as many as it can.
    #[error("the device refused the pipe")]
    Refused,
    #[error("the device did not answer in time")]
    Timeout,
    /// This end of the link may not do that.
    #[error("not possible in this role")]
    Role,
    /// Not an open descriptor.
    #[error("not a valid descriptor")]
    Descriptor,
}

impl From<UsbError> for UsbLinkError {
    fn from(error: UsbError) -> Self {
        match error {
            UsbError::Refused(_) => Self::Refused,
            UsbError::Timeout => Self::Timeout,
            UsbError::Protocol => Self::Role,
            UsbError::ServiceMissing | UsbError::Io(_) => Self::Closed,
        }
    }
}

/// A pipe a device accepted, and the service the host asked for.
#[derive(uniffi::Record)]
pub struct AcceptedUsbPipe {
    pub service: UsbService,
    pub pipe: Arc<UsbPipeHandle>,
}

/// One accessory connection, framed into pipes
/// ([`crate::aoa::AccessoryLink`]). Dropping it ends the link.
#[derive(uniffi::Object)]
pub struct UsbAccessoryLink {
    link: Arc<AccessoryLink>,
    device: String,
}

#[uniffi::export]
impl UsbAccessoryLink {
    /// Starts the link on `io`, the accessory's raw byte stream. `device` is
    /// this machine's label for the other end: local display only, never
    /// identity.
    #[uniffi::constructor]
    pub fn start(io: Arc<dyn ByteDuplex>, role: LinkRole, device: String) -> Arc<Self> {
        let link = AccessoryLink::start_with(pipe_from_duplex(io), role, |task| {
            // Without a runtime the tasks are dropped, which ends the link.
            streamcore_transport::spawn(task);
        });
        Arc::new(Self {
            link: Arc::new(link),
            device,
        })
    }

    /// Host: opens a pipe to `service` on the device, for
    /// `SecureListener::accept_pipe`. (Not named `open`: a Swift keyword.)
    pub async fn open_pipe(&self, service: UsbService) -> Result<Arc<UsbPipeHandle>, UsbLinkError> {
        let (answer, result) = oneshot::channel();
        let link = self.link.clone();
        // The open's deadline needs the runtime's timer.
        if !streamcore_transport::spawn(async move {
            let _ = answer.send(link.open(service).await);
        }) {
            return Err(UsbLinkError::Closed);
        }
        let io = result.await.map_err(|_| UsbLinkError::Closed)??;
        Ok(self.handle(io))
    }

    /// Device: the next pipe the host opened, for `connect_secure_pipe`;
    /// `None` when the link ended.
    pub async fn accept(&self) -> Option<AcceptedUsbPipe> {
        let accepted = self.link.accept().await?;
        Some(AcceptedUsbPipe {
            service: accepted.service,
            pipe: self.handle(accepted.io),
        })
    }

    /// Resolves when the link has ended.
    pub async fn closed(&self) {
        self.link.closed().await;
    }

    pub fn is_closed(&self) -> bool {
        self.link.is_closed()
    }

    /// Ends the link and every pipe on it. (Not named `close`: generated
    /// Kotlin objects already have one.)
    pub fn end(&self) {
        self.link.close();
    }
}

// Its own block: the export macro cannot leave one method of a block out on
// a platform without descriptors.
#[cfg(unix)]
#[uniffi::export]
impl UsbAccessoryLink {
    /// [`Self::start`] on an open descriptor (an Android accessory's, from
    /// `ParcelFileDescriptor.detachFd()`), which Rust takes over, reads and
    /// writes itself and closes: no transfer crosses the binding.
    #[uniffi::constructor]
    pub fn start_descriptor(
        descriptor: i32,
        role: LinkRole,
        device: String,
    ) -> Result<Arc<Self>, UsbLinkError> {
        let io = streamcore_transport::duplex::DescriptorDuplex::adopt(descriptor)
            .ok_or(UsbLinkError::Descriptor)?;
        Ok(Self::start(io, role, device))
    }
}

impl UsbAccessoryLink {
    fn handle(&self, io: streamcore_transport::transport::PipeIo) -> Arc<UsbPipeHandle> {
        UsbPipeHandle::new(UsbPipe {
            io,
            medium: Medium::UsbAoa,
            device: self.device.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Relayed pipes: a host keeps them open through adb or the device
// multiplexer, and a device listens for them on its loopback interface.

/// Why a provider or a listener could not do what was asked.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UsbProviderError {
    /// The provider's local service is not running or not installed.
    #[error("the USB service is not available")]
    ServiceMissing,
    /// A port list, a name or an address the provider cannot use, or a
    /// service that broke its protocol.
    #[error("not valid for this provider")]
    Invalid,
    #[error("the USB service refused")]
    Refused,
    #[error("the USB service did not answer in time")]
    Timeout,
    #[error("USB I/O failed")]
    Io,
    /// The network runtime is not available.
    #[error("the runtime is unavailable")]
    Runtime,
}

impl From<UsbError> for UsbProviderError {
    fn from(error: UsbError) -> Self {
        match error {
            UsbError::ServiceMissing => Self::ServiceMissing,
            UsbError::Protocol => Self::Invalid,
            // The service's reason may name the device: it stays here.
            UsbError::Refused(_) => Self::Refused,
            UsbError::Timeout => Self::Timeout,
            UsbError::Io(_) => Self::Io,
        }
    }
}

/// Runs `future` on the transport's runtime (sockets and timers need it) and
/// returns its output.
async fn on_runtime<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, UsbProviderError> {
    let (done, result) = oneshot::channel();
    if !streamcore_transport::spawn(async move {
        let _ = done.send(future.await);
    }) {
        return Err(UsbProviderError::Runtime);
    }
    result.await.map_err(|_| UsbProviderError::Runtime)
}

/// The device side of relayed pipes ([`crate::device::DeviceListener`]): the
/// host's provider connects to a port on this device's loopback interface,
/// and the device dials the host over each pipe it accepts.
#[derive(uniffi::Object)]
pub struct UsbDeviceListener {
    listener: Arc<crate::device::DeviceListener>,
    port: u16,
}

#[uniffi::export]
impl UsbDeviceListener {
    /// Listens on the first free port of `ports` (the host's provider probes
    /// the same list in the same order). `medium` is what carries the host's
    /// pipes to this device: `UsbMux` or `UsbAdb`.
    #[uniffi::constructor]
    pub async fn bind(ports: Vec<u16>, medium: Medium) -> Result<Arc<Self>, UsbProviderError> {
        on_runtime(async move {
            let listener = crate::device::DeviceListener::bind(&ports, medium).await?;
            let port = listener.port()?;
            Ok(Arc::new(Self {
                listener: Arc::new(listener),
                port,
            }))
        })
        .await?
    }

    /// The port this listener got.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The next pipe, for `connect_secure_pipe`. While the listener's places
    /// are all held by live pipes, this waits.
    pub async fn accept(&self) -> Result<Arc<UsbPipeHandle>, UsbProviderError> {
        let listener = self.listener.clone();
        let pipe = on_runtime(async move { listener.accept().await }).await??;
        Ok(UsbPipeHandle::new(pipe))
    }
}

/// A host's kept pipes ([`crate::keeper::keep_pipes`]): one per cabled device,
/// opened when the device appears, handed to the listener (which holds it
/// idle until the device dials) and reopened when it ends. Their channels
/// arrive through the listener's `accept`. Dropping this, or `stop`, ends the
/// keeping; pipes the listener already holds are the listener's.
#[derive(uniffi::Object)]
pub struct UsbHostPipes {
    stop: tokio::sync::watch::Sender<bool>,
}

impl UsbHostPipes {
    /// Keeps pipes from any provider (for Rust callers and tests).
    pub fn keep(
        provider: Arc<dyn crate::UsbPipeProvider>,
        listener: Arc<streamcore_transport::channel::SecureListener>,
    ) -> Result<Arc<Self>, UsbProviderError> {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let started = streamcore_transport::spawn(async move {
            let keeping = crate::keeper::keep_pipes(
                provider,
                listener,
                None,
                crate::keeper::KeeperPolicy::default(),
            );
            tokio::select! {
                () = keeping => {}
                _ = stopped.wait_for(|stopped| *stopped) => {}
            }
        });
        if !started {
            return Err(UsbProviderError::Runtime);
        }
        Ok(Arc::new(Self { stop }))
    }
}

/// A provider seen through its pairing service only: whatever is asked for,
/// the pipe goes to the device's pairing listener.
struct PairingOnly(Arc<dyn crate::UsbPipeProvider>);

#[async_trait::async_trait]
impl crate::UsbPipeProvider for PairingOnly {
    async fn next_event(&self) -> Option<crate::UsbEvent> {
        self.0.next_event().await
    }
    async fn open(
        &self,
        device: &crate::UsbDevice,
        _service: UsbService,
    ) -> Result<streamcore_transport::transport::PipeIo, UsbError> {
        self.0.open(device, UsbService::Pairing).await
    }
}

impl UsbHostPipes {
    /// Keeps pairing pipes from any provider while `service`'s window is
    /// open (for Rust callers and tests): one per cabled device, to its
    /// pairing listener. It ends when the window closes or on `stop`.
    pub fn keep_pairing(
        provider: Arc<dyn crate::UsbPipeProvider>,
        service: Arc<streamcore_transport::pairing_transport::PairingService>,
    ) -> Result<Arc<Self>, UsbProviderError> {
        if service.is_closed() {
            return Err(UsbProviderError::Refused);
        }
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let window = service.clone();
        let started = streamcore_transport::spawn(async move {
            let keeping = crate::keeper::keep_pipes(
                Arc::new(PairingOnly(provider)),
                service,
                None,
                crate::keeper::KeeperPolicy::default(),
            );
            tokio::select! {
                () = keeping => {}
                _ = stopped.wait_for(|stopped| *stopped) => {}
                () = window.closed() => {}
            }
        });
        if !started {
            return Err(UsbProviderError::Runtime);
        }
        Ok(Arc::new(Self { stop }))
    }
}

#[uniffi::export]
impl UsbHostPipes {
    /// Pipes through the Apple mobile device multiplexer at its usual
    /// address, to `session_port` on each cabled device. `program` is the
    /// client name the service logs (letters, digits, `.`, `_`, `-`).
    #[uniffi::constructor]
    pub fn usbmux(
        listener: Arc<streamcore_transport::channel::SecureListener>,
        program: String,
        session_port: u16,
    ) -> Result<Arc<Self>, UsbProviderError> {
        use crate::usbmux::{MuxAddress, MuxPorts, MuxProvider, MuxService};
        if session_port == 0 {
            return Err(UsbProviderError::Invalid);
        }
        let service = MuxService::new(MuxAddress::platform_default(), &program)?;
        let ports = MuxPorts {
            session: session_port,
            // No pairing sink is kept, so this port is never opened.
            pairing: session_port,
        };
        Self::keep(Arc::new(MuxProvider::new(service, ports)), listener)
    }

    /// Pipes through a running Android debug bridge server at its usual
    /// address, to `session_port` on each cabled device. No server is started
    /// or stopped.
    #[uniffi::constructor]
    pub fn adb(
        listener: Arc<streamcore_transport::channel::SecureListener>,
        session_port: u16,
    ) -> Result<Arc<Self>, UsbProviderError> {
        use crate::adb::{AdbEndpoints, AdbProvider, AdbServer, AdbTarget, DEFAULT_SERVER};
        if session_port == 0 {
            return Err(UsbProviderError::Invalid);
        }
        let address = DEFAULT_SERVER
            .parse()
            .map_err(|_| UsbProviderError::Invalid)?;
        let endpoints = AdbEndpoints {
            session: AdbTarget::Tcp(session_port),
            pairing: AdbTarget::Tcp(session_port),
        };
        Self::keep(
            Arc::new(AdbProvider::new(AdbServer::new(address)?, endpoints)),
            listener,
        )
    }

    /// Pairing pipes through the device multiplexer, to `pairing_port` on
    /// each cabled device, for as long as `service`'s window is open: the
    /// keeping stops by itself when the window closes, so no pairing pipe
    /// exists outside one.
    #[uniffi::constructor]
    pub fn usbmux_pairing(
        service: Arc<streamcore_transport::pairing_transport::PairingService>,
        program: String,
        pairing_port: u16,
    ) -> Result<Arc<Self>, UsbProviderError> {
        use crate::usbmux::{MuxAddress, MuxPorts, MuxProvider, MuxService};
        if pairing_port == 0 {
            return Err(UsbProviderError::Invalid);
        }
        let mux = MuxService::new(MuxAddress::platform_default(), &program)?;
        let ports = MuxPorts {
            session: pairing_port,
            pairing: pairing_port,
        };
        Self::keep_pairing(Arc::new(MuxProvider::new(mux, ports)), service)
    }

    /// Pairing pipes through a running Android debug bridge server, to
    /// `pairing_port` on each cabled device, for as long as `service`'s
    /// window is open.
    #[uniffi::constructor]
    pub fn adb_pairing(
        service: Arc<streamcore_transport::pairing_transport::PairingService>,
        pairing_port: u16,
    ) -> Result<Arc<Self>, UsbProviderError> {
        use crate::adb::{AdbEndpoints, AdbProvider, AdbServer, AdbTarget, DEFAULT_SERVER};
        if pairing_port == 0 {
            return Err(UsbProviderError::Invalid);
        }
        let address = DEFAULT_SERVER
            .parse()
            .map_err(|_| UsbProviderError::Invalid)?;
        let endpoints = AdbEndpoints {
            session: AdbTarget::Tcp(pairing_port),
            pairing: AdbTarget::Tcp(pairing_port),
        };
        Self::keep_pairing(
            Arc::new(AdbProvider::new(AdbServer::new(address)?, endpoints)),
            service,
        )
    }

    /// Stops keeping pipes.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}

impl Drop for UsbHostPipes {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
