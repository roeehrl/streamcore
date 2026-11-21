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
    /// `SecureListener::accept_pipe`.
    pub async fn open(&self, service: UsbService) -> Result<Arc<UsbPipeHandle>, UsbLinkError> {
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

impl UsbAccessoryLink {
    fn handle(&self, io: streamcore_transport::transport::PipeIo) -> Arc<UsbPipeHandle> {
        UsbPipeHandle::new(UsbPipe {
            io,
            medium: Medium::UsbAoa,
            device: self.device.clone(),
        })
    }
}
