//! Pipes through the Apple mobile device multiplexer: a client of the service
//! the operating system or the device vendor's software runs (a Unix socket on
//! macOS and Linux, a loopback TCP port on Windows). It speaks the service's
//! property-list protocol: `Listen` for attach and detach, and `Connect` to
//! open a pipe to a TCP port on the device's loopback interface.
//!
//! It never starts or installs the service: a missing one is
//! [`UsbError::ServiceMissing`], and the product guides the person. The
//! service and everything it reports are untrusted input: message sizes are
//! bounded, parsing is defensive, and every exchange has a deadline.

use crate::{
    UsbDevice, UsbError, UsbEvent, UsbPipeProvider, UsbService,
    plist::{self, Field, Plist},
};
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use streamcore_transport::transport::{Medium, PipeIo, PipeStream};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};

/// The service's Unix socket on macOS and Linux.
pub const UNIX_SOCKET: &str = "/var/run/usbmuxd";
/// The service's loopback address on Windows.
pub const WINDOWS_ADDRESS: &str = "127.0.0.1:27015";
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest message accepted: an attach event or a device list.
const MAX_MESSAGE: usize = 256 * 1024;
const HEADER: usize = 16;
/// Protocol version 1 with property-list payloads.
const VERSION_PLIST: u32 = 1;
const MESSAGE_PLIST: u32 = 8;
const MAX_LABEL: usize = 128;

/// Where the multiplexer service listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuxAddress {
    #[cfg(unix)]
    Unix(std::path::PathBuf),
    /// Must be a loopback address: the protocol is unauthenticated.
    Tcp(SocketAddr),
}

impl MuxAddress {
    /// The platform's usual address.
    pub fn platform_default() -> Self {
        #[cfg(unix)]
        {
            MuxAddress::Unix(UNIX_SOCKET.into())
        }
        #[cfg(not(unix))]
        {
            // The constant is a valid socket address.
            MuxAddress::Tcp(SocketAddr::from(([127, 0, 0, 1], 27015)))
        }
    }
}

async fn deadline<T>(work: impl Future<Output = Result<T, UsbError>>) -> Result<T, UsbError> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, work)
        .await
        .map_err(|_| UsbError::Timeout)?
}

type Stream = Box<dyn PipeStream>;

fn missing(error: std::io::Error) -> UsbError {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
            UsbError::ServiceMissing
        }
        _ => UsbError::Io(error),
    }
}

async fn write_message(stream: &mut Stream, tag: u32, payload: &[u8]) -> Result<(), UsbError> {
    let length = u32::try_from(HEADER + payload.len()).map_err(|_| UsbError::Protocol)?;
    let mut message = Vec::with_capacity(HEADER + payload.len());
    for word in [length, VERSION_PLIST, MESSAGE_PLIST, tag] {
        message.extend(word.to_le_bytes());
    }
    message.extend_from_slice(payload);
    stream.write_all(&message).await?;
    Ok(())
}

async fn read_message(stream: &mut Stream) -> Result<Plist, UsbError> {
    let mut header = [0u8; HEADER];
    stream.read_exact(&mut header).await?;
    let word = |index: usize| {
        u32::from_le_bytes([
            header[index],
            header[index + 1],
            header[index + 2],
            header[index + 3],
        ])
    };
    let length = word(0) as usize;
    if !(HEADER..=MAX_MESSAGE).contains(&length) || word(8) != MESSAGE_PLIST {
        return Err(UsbError::Protocol);
    }
    let mut payload = vec![0; length - HEADER];
    stream.read_exact(&mut payload).await?;
    plist::parse(&payload).ok_or(UsbError::Protocol)
}

/// The service's `Result` reply: zero, or a refusal.
fn check_result(reply: &Plist) -> Result<(), UsbError> {
    if reply.string("MessageType") != Some("Result") {
        return Err(UsbError::Protocol);
    }
    match reply.integer("Number").ok_or(UsbError::Protocol)? {
        0 => Ok(()),
        2 => Err(UsbError::Refused("the device is not attached".into())),
        3 => Err(UsbError::Refused("closed".into())),
        6 => Err(UsbError::Refused("unsupported protocol version".into())),
        number => Err(UsbError::Refused(format!("result {number}"))),
    }
}

/// A device label usable for display and as a map key.
fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= MAX_LABEL && label.bytes().all(|b| b.is_ascii_graphic())
}

/// A cabled device in an `Attached` message or a list entry: its service-side
/// number and its label. Devices the service reaches over the network are not
/// a cable and are left out.
fn cabled_device(entry: &Plist) -> Option<(u32, String)> {
    let id = u32::try_from(entry.integer("DeviceID")?).ok()?;
    let properties = entry.get("Properties")?;
    let label = properties.string("SerialNumber")?;
    (properties.string("ConnectionType") == Some("USB") && valid_label(label))
        .then(|| (id, label.to_owned()))
}

/// A client of one multiplexer service.
#[derive(Clone, Debug)]
pub struct MuxService {
    address: MuxAddress,
    /// Sent as the client's name; the service shows it in its own logs.
    program: String,
}

impl MuxService {
    pub fn new(address: MuxAddress, program: &str) -> Result<Self, UsbError> {
        let plain = !program.is_empty()
            && program.len() <= 64
            && program
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        let local = match &address {
            #[cfg(unix)]
            MuxAddress::Unix(_) => true,
            MuxAddress::Tcp(address) => address.ip().is_loopback() && address.port() != 0,
        };
        if plain && local {
            Ok(Self {
                address,
                program: program.to_owned(),
            })
        } else {
            Err(UsbError::Protocol)
        }
    }

    async fn connect(&self) -> Result<Stream, UsbError> {
        Ok(match &self.address {
            #[cfg(unix)]
            MuxAddress::Unix(path) => Box::new(
                tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(missing)?,
            ),
            MuxAddress::Tcp(address) => {
                let stream = TcpStream::connect(address).await.map_err(missing)?;
                stream.set_nodelay(true)?;
                Box::new(stream)
            }
        })
    }

    fn request(&self, kind: &str, extra: &[(&str, Field)]) -> Vec<u8> {
        let mut fields = vec![
            ("MessageType", Field::Text(kind)),
            ("ClientVersionString", Field::Text(&self.program)),
            ("ProgName", Field::Text(&self.program)),
            ("kLibUSBMuxVersion", Field::Number(3)),
        ];
        fields.extend(extra.iter().map(|(key, value)| {
            (
                *key,
                match value {
                    Field::Text(text) => Field::Text(text),
                    Field::Number(number) => Field::Number(*number),
                },
            )
        }));
        plist::request(&fields)
    }

    /// The cabled devices now, as (service number, label).
    pub async fn devices(&self) -> Result<Vec<(u32, String)>, UsbError> {
        deadline(async {
            let mut stream = self.connect().await?;
            write_message(&mut stream, 1, &self.request("ListDevices", &[])).await?;
            let reply = read_message(&mut stream).await?;
            let list = reply.array("DeviceList").ok_or(UsbError::Protocol)?;
            Ok(list.iter().filter_map(cabled_device).collect())
        })
        .await
    }

    /// Opens a pipe to the TCP `port` on the device with the service number
    /// `device`. On success the stream carries the device socket's bytes only.
    pub async fn open(&self, device: u32, port: u16) -> Result<PipeIo, UsbError> {
        if port == 0 {
            return Err(UsbError::Protocol);
        }
        deadline(async {
            let mut stream = self.connect().await?;
            // The service takes the port in network byte order.
            let wire_port = i64::from(port.swap_bytes());
            let request = self.request(
                "Connect",
                &[
                    ("DeviceID", Field::Number(i64::from(device))),
                    ("PortNumber", Field::Number(wire_port)),
                ],
            );
            write_message(&mut stream, 2, &request).await?;
            check_result(&read_message(&mut stream).await?)?;
            Ok(stream)
        })
        .await
    }

    /// Starts listening: the stream then delivers `Attached` and `Detached`.
    async fn listen(&self) -> Result<Stream, UsbError> {
        deadline(async {
            let mut stream = self.connect().await?;
            write_message(&mut stream, 3, &self.request("Listen", &[])).await?;
            check_result(&read_message(&mut stream).await?)?;
            Ok(stream)
        })
        .await
    }
}

/// The device-side TCP ports a product listens on, per service.
#[derive(Clone, Copy, Debug)]
pub struct MuxPorts {
    pub session: u16,
    pub pairing: u16,
}

struct Listening {
    stream: Option<Stream>,
    /// Attached cabled devices: service number to label.
    known: BTreeMap<u32, String>,
    queued: std::collections::VecDeque<UsbEvent>,
}

/// The multiplexer [`UsbPipeProvider`].
pub struct MuxProvider {
    service: MuxService,
    ports: MuxPorts,
    listening: Mutex<Listening>,
    /// Label to service number, for `open`: its own lock, because
    /// `next_event` holds `listening` while it waits.
    numbers: std::sync::Mutex<BTreeMap<String, u32>>,
}

impl MuxProvider {
    pub fn new(service: MuxService, ports: MuxPorts) -> Self {
        Self {
            service,
            ports,
            listening: Mutex::new(Listening {
                stream: None,
                known: BTreeMap::new(),
                queued: Default::default(),
            }),
            numbers: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    fn device(label: &str) -> UsbDevice {
        UsbDevice {
            medium: Medium::UsbMux,
            label: label.to_owned(),
        }
    }

    fn remember(&self, label: &str, number: Option<u32>) {
        if let Ok(mut numbers) = self.numbers.lock() {
            match number {
                Some(number) => numbers.insert(label.to_owned(), number),
                None => numbers.remove(label),
            };
        }
    }
}

#[async_trait::async_trait]
impl UsbPipeProvider for MuxProvider {
    async fn next_event(&self) -> Option<UsbEvent> {
        // Held across the wait for the next message, which has no deadline:
        // nothing else may take this lock (`open` uses `numbers`).
        let mut listening = self.listening.lock().await;
        loop {
            if let Some(event) = listening.queued.pop_front() {
                return Some(event);
            }
            if listening.stream.is_none() {
                listening.stream = Some(self.service.listen().await.ok()?);
            }
            let message = match listening.stream.as_mut() {
                Some(stream) => read_message(stream).await,
                None => return None,
            };
            let Ok(message) = message else {
                // The service went away: every device is gone, then the end.
                listening.stream = None;
                let gone = std::mem::take(&mut listening.known);
                if gone.is_empty() {
                    return None;
                }
                for label in gone.values() {
                    self.remember(label, None);
                    listening
                        .queued
                        .push_back(UsbEvent::Detached(Self::device(label)));
                }
                continue;
            };
            match message.string("MessageType") {
                Some("Attached") => {
                    if let Some((number, label)) = cabled_device(&message)
                        && !listening.known.values().any(|known| *known == label)
                    {
                        self.remember(&label, Some(number));
                        listening
                            .queued
                            .push_back(UsbEvent::Attached(Self::device(&label)));
                        listening.known.insert(number, label);
                    }
                }
                Some("Detached") => {
                    let number = message
                        .integer("DeviceID")
                        .and_then(|number| u32::try_from(number).ok());
                    if let Some(label) = number.and_then(|n| listening.known.remove(&n)) {
                        self.remember(&label, None);
                        listening
                            .queued
                            .push_back(UsbEvent::Detached(Self::device(&label)));
                    }
                }
                // Other notices (pairing records, for example) are not ours.
                _ => {}
            }
        }
    }

    async fn open(&self, device: &UsbDevice, service: UsbService) -> Result<PipeIo, UsbError> {
        if device.medium != Medium::UsbMux {
            return Err(UsbError::Protocol);
        }
        let number = self
            .numbers
            .lock()
            .ok()
            .and_then(|numbers| numbers.get(&device.label).copied())
            .ok_or_else(|| UsbError::Refused("the device is not attached".into()))?;
        let port = match service {
            UsbService::Session => self.ports.session,
            UsbService::Pairing => self.ports.pairing,
        };
        self.service.open(number, port).await
    }
}
