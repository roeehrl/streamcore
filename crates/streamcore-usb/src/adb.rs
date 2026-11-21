//! Pipes through the Android debug bridge: this speaks the host protocol to an
//! adb server that is already running (`host:version`, `host:track-devices-l`,
//! and per pipe `host:transport:<serial>` then `tcp:<port>` or
//! `localabstract:<name>`). The host opens the pipe (adb forward's direction).
//!
//! It never starts, stops or replaces a server: the product owns that choice
//! and gives the server's address. The server and everything it reports are
//! untrusted input: lengths are bounded, text is parsed defensively, and every
//! exchange has a deadline.

use crate::{UsbDevice, UsbError, UsbEvent, UsbPipeProvider, UsbService};
use std::{collections::BTreeSet, net::SocketAddr, time::Duration};
use streamcore_transport::transport::{Medium, PipeIo};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};

/// The adb server's default address.
pub const DEFAULT_SERVER: &str = "127.0.0.1:5037";
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest reply text accepted. The protocol's four hex digits allow
/// 65,535 bytes; a device listing is about a hundred bytes per device.
const MAX_REPLY: usize = 16 * 1024;
const MAX_SERIAL: usize = 128;

/// Where on the device a pipe ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdbTarget {
    /// A TCP port on the device's loopback interface.
    Tcp(u16),
    /// An abstract-namespace Unix socket on the device.
    LocalAbstract(String),
}

impl AdbTarget {
    fn service(&self) -> Result<String, UsbError> {
        match self {
            AdbTarget::Tcp(0) => Err(UsbError::Protocol),
            AdbTarget::Tcp(port) => Ok(format!("tcp:{port}")),
            AdbTarget::LocalAbstract(name) => {
                let plain = !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
                if plain {
                    Ok(format!("localabstract:{name}"))
                } else {
                    Err(UsbError::Protocol)
                }
            }
        }
    }
}

/// A serial usable in a request: bounded, printable, and without the request
/// syntax's separator.
fn valid_serial(serial: &str) -> bool {
    !serial.is_empty()
        && serial.len() <= MAX_SERIAL
        && serial
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b':' && b != b';')
}

/// The cabled devices in a `devices-l` listing that are ready (`device`
/// state). Wireless-debugging and network entries are left out (their serials
/// carry a host and port or an mDNS service name), and so are emulators: none
/// of them is a cable. The rule goes by the serial, not by the listing's
/// `usb:` field, because not every platform's server prints that field.
pub fn parse_devices(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (serial, state) = (fields.next()?, fields.next()?);
            let cabled = valid_serial(serial)
                && !serial.contains("._tcp")
                && !serial.contains('.')
                && !serial.starts_with("emulator-");
            (state == "device" && cabled).then(|| serial.to_owned())
        })
        .collect()
}

async fn deadline<T>(work: impl Future<Output = Result<T, UsbError>>) -> Result<T, UsbError> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, work)
        .await
        .map_err(|_| UsbError::Timeout)?
}

async fn read_hex4(stream: &mut TcpStream) -> Result<usize, UsbError> {
    let mut digits = [0u8; 4];
    stream.read_exact(&mut digits).await?;
    let text = std::str::from_utf8(&digits).map_err(|_| UsbError::Protocol)?;
    usize::from_str_radix(text, 16).map_err(|_| UsbError::Protocol)
}

async fn read_text(stream: &mut TcpStream) -> Result<String, UsbError> {
    let length = read_hex4(stream).await?;
    if length > MAX_REPLY {
        return Err(UsbError::Protocol);
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).map_err(|_| UsbError::Protocol)
}

/// Sends one request and reads its status: `OKAY`, or `FAIL` with a reason.
async fn request(stream: &mut TcpStream, payload: &str) -> Result<(), UsbError> {
    if payload.len() > 0xffff {
        return Err(UsbError::Protocol);
    }
    stream
        .write_all(format!("{:04x}{payload}", payload.len()).as_bytes())
        .await?;
    let mut status = [0u8; 4];
    stream.read_exact(&mut status).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let mut reason = read_text(stream).await.unwrap_or_default();
            reason.truncate(200);
            Err(UsbError::Refused(reason))
        }
        _ => Err(UsbError::Protocol),
    }
}

/// A client of one adb server.
#[derive(Clone, Debug)]
pub struct AdbServer {
    address: SocketAddr,
}

impl AdbServer {
    /// `address` must be a loopback address: the adb host protocol is
    /// unauthenticated and is only ever spoken to this machine.
    pub fn new(address: SocketAddr) -> Result<Self, UsbError> {
        if address.ip().is_loopback() && address.port() != 0 {
            Ok(Self { address })
        } else {
            Err(UsbError::Protocol)
        }
    }

    async fn connect(&self) -> Result<TcpStream, UsbError> {
        let stream = TcpStream::connect(self.address).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::ConnectionRefused {
                UsbError::ServiceMissing
            } else {
                UsbError::Io(error)
            }
        })?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    /// The server's protocol version (`host:version`).
    pub async fn version(&self) -> Result<u32, UsbError> {
        deadline(async {
            let mut stream = self.connect().await?;
            request(&mut stream, "host:version").await?;
            let text = read_text(&mut stream).await?;
            u32::from_str_radix(&text, 16).map_err(|_| UsbError::Protocol)
        })
        .await
    }

    /// The ready cabled devices now (`host:devices-l`).
    pub async fn devices(&self) -> Result<BTreeSet<String>, UsbError> {
        deadline(async {
            let mut stream = self.connect().await?;
            request(&mut stream, "host:devices-l").await?;
            Ok(parse_devices(&read_text(&mut stream).await?))
        })
        .await
    }

    /// Opens a pipe to `target` on the device `serial`. On success the stream
    /// carries the device socket's bytes and nothing else.
    pub async fn open(&self, serial: &str, target: &AdbTarget) -> Result<TcpStream, UsbError> {
        if !valid_serial(serial) {
            return Err(UsbError::Protocol);
        }
        let service = target.service()?;
        deadline(async {
            let mut stream = self.connect().await?;
            request(&mut stream, &format!("host:transport:{serial}")).await?;
            request(&mut stream, &service).await?;
            Ok(stream)
        })
        .await
    }

    /// Starts tracking devices (`host:track-devices-l`): the stream then
    /// delivers a full listing at once and after every change.
    async fn track(&self) -> Result<TcpStream, UsbError> {
        deadline(async {
            let mut stream = self.connect().await?;
            request(&mut stream, "host:track-devices-l").await?;
            Ok(stream)
        })
        .await
    }
}

/// The device-side endpoints a product listens on, per service.
#[derive(Clone, Debug)]
pub struct AdbEndpoints {
    pub session: AdbTarget,
    pub pairing: AdbTarget,
}

struct Tracking {
    stream: Option<TcpStream>,
    known: BTreeSet<String>,
    /// Events computed from the last listing and not yet returned.
    queued: std::collections::VecDeque<UsbEvent>,
}

/// The adb [`UsbPipeProvider`]: attach and detach events from the server's
/// device tracking, and pipes to the product's device-side endpoints.
pub struct AdbProvider {
    server: AdbServer,
    endpoints: AdbEndpoints,
    tracking: Mutex<Tracking>,
}

impl AdbProvider {
    pub fn new(server: AdbServer, endpoints: AdbEndpoints) -> Self {
        Self {
            server,
            endpoints,
            tracking: Mutex::new(Tracking {
                stream: None,
                known: BTreeSet::new(),
                queued: Default::default(),
            }),
        }
    }

    fn device(serial: &str) -> UsbDevice {
        UsbDevice {
            medium: Medium::UsbAdb,
            label: serial.to_owned(),
        }
    }
}

#[async_trait::async_trait]
impl UsbPipeProvider for AdbProvider {
    async fn next_event(&self) -> Option<UsbEvent> {
        // Held across the wait for the next listing, which has no deadline:
        // nothing else may take this lock (`open` does not).
        let mut tracking = self.tracking.lock().await;
        loop {
            if let Some(event) = tracking.queued.pop_front() {
                return Some(event);
            }
            if tracking.stream.is_none() {
                tracking.stream = Some(self.server.track().await.ok()?);
            }
            // No deadline here: a listing arrives only when something changes.
            let listing = match tracking.stream.as_mut() {
                Some(stream) => read_text(stream).await,
                None => return None,
            };
            let Ok(listing) = listing else {
                // The server went away: every device is gone, then the end.
                tracking.stream = None;
                let gone = std::mem::take(&mut tracking.known);
                if gone.is_empty() {
                    return None;
                }
                tracking
                    .queued
                    .extend(gone.iter().map(|s| UsbEvent::Detached(Self::device(s))));
                continue;
            };
            let now = parse_devices(&listing);
            let detached: Vec<_> = tracking.known.difference(&now).cloned().collect();
            let attached: Vec<_> = now.difference(&tracking.known).cloned().collect();
            tracking
                .queued
                .extend(detached.iter().map(|s| UsbEvent::Detached(Self::device(s))));
            tracking
                .queued
                .extend(attached.iter().map(|s| UsbEvent::Attached(Self::device(s))));
            tracking.known = now;
        }
    }

    async fn open(&self, device: &UsbDevice, service: UsbService) -> Result<PipeIo, UsbError> {
        if device.medium != Medium::UsbAdb {
            return Err(UsbError::Protocol);
        }
        let target = match service {
            UsbService::Session => &self.endpoints.session,
            UsbService::Pairing => &self.endpoints.pairing,
        };
        Ok(Box::new(self.server.open(&device.label, target).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ready_cabled_devices_are_listed() {
        let listing = "\
SERIAL0001            device usb:1-1 product:x model:y device:z transport_id:3
SERIAL0003             unauthorized usb:1-2 transport_id:4
192.0.2.20:5555      device product:x model:y device:z transport_id:5
adb-SERIAL0001-AbCdEf._adb-tls-connect._tcp device product:x transport_id:6
emulator-5554          offline transport_id:7
SERIAL0002             device usb:2-1 transport_id:8
emulator-5556          device product:x model:y device:z transport_id:9

garbage
";
        let devices: Vec<_> = parse_devices(listing).into_iter().collect();
        assert_eq!(devices, ["SERIAL0001", "SERIAL0002"]);
    }

    #[test]
    fn serials_and_targets_that_could_alter_a_request_are_refused() {
        assert!(!valid_serial(""));
        assert!(!valid_serial("a:b"));
        assert!(!valid_serial("a b"));
        assert!(!valid_serial(&"x".repeat(MAX_SERIAL + 1)));
        assert!(valid_serial("SERIAL0001"));
        assert!(AdbTarget::Tcp(0).service().is_err());
        assert_eq!(AdbTarget::Tcp(47820).service().unwrap(), "tcp:47820");
        assert!(AdbTarget::LocalAbstract("a;b".into()).service().is_err());
        assert_eq!(
            AdbTarget::LocalAbstract("stream.session".into())
                .service()
                .unwrap(),
            "localabstract:stream.session"
        );
    }

    #[test]
    fn the_server_must_be_on_loopback() {
        assert!(AdbServer::new("192.0.2.2:5037".parse().unwrap()).is_err());
        assert!(AdbServer::new("127.0.0.1:0".parse().unwrap()).is_err());
        assert!(AdbServer::new(DEFAULT_SERVER.parse().unwrap()).is_ok());
    }
}
