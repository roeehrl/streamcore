//! USB pipe providers (STREAMCORE-VIDEO 8.4): each opens byte streams between
//! this machine and a cabled device and hands them to the secure transport
//! (`SecureListener::accept_stream` on the host), which runs the same pinned
//! mutual TLS on them as on the network.
//!
//! A cable is not authentication. Nothing a provider reports (serials, labels,
//! states) feeds a trust decision, and a device label is private to this
//! machine: show it locally, never send or log it where it leaves the machine.

pub mod adb;

use streamcore_transport::transport::{Medium, PipeIo};

/// A cabled device as a provider sees it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsbDevice {
    pub medium: Medium,
    /// The provider's label (a serial or similar): local display and routing
    /// only, never identity.
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbEvent {
    Attached(UsbDevice),
    Detached(UsbDevice),
}

/// Which listener on the device a pipe goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbService {
    Session,
    Pairing,
}

#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    /// The provider's local service is not running or not installed.
    #[error("the USB service is not available")]
    ServiceMissing,
    /// The service answered, but not as its protocol requires.
    #[error("the USB service broke its protocol")]
    Protocol,
    /// The service refused the request (a device gone, a port closed). The
    /// text is the service's own reason and may name the device: like a device
    /// label it is for this machine only, never for a log that leaves it.
    #[error("the USB service refused: {0}")]
    Refused(String),
    #[error("the USB service did not answer in time")]
    Timeout,
    #[error("USB I/O failed")]
    Io(#[from] std::io::Error),
}

/// A host-side source of pipes to cabled devices.
#[async_trait::async_trait]
pub trait UsbPipeProvider: Send + Sync {
    /// The next attach or detach, or `None` when the provider has stopped (its
    /// service went away). Every device then counts as detached.
    async fn next_event(&self) -> Option<UsbEvent>;
    /// Opens a pipe to `service` on `device`.
    async fn open(&self, device: &UsbDevice, service: UsbService) -> Result<PipeIo, UsbError>;
}
