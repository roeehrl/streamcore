//! The same authenticated session over QUIC or TLS/TCP. Path racing selects a
//! fully authenticated, negotiated connection; it never races application input.

use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::identity::{
    IdentityError, PEER_DNS_NAME, PeerIdentity, SESSION_ALPN, certificate_fingerprint,
};
use crate::protocol::{self, Hello, ProtocolError};
use crate::tuning::{TransportTuning, quic_transport, transport_tuning};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PATHS: usize = 16;
const MAX_PENDING_HANDSHAKES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    TlsTcp,
    /// The TLS/TCP path's framing over a byte stream that is not a TCP socket
    /// this crate opened: a pipe a USB provider handed over.
    TlsStream {
        medium: Medium,
    },
}

/// What carries a [`TransportKind::TlsStream`] session. A cable is not
/// authentication: every medium runs the same pinned mutual TLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum Medium {
    /// A USB network interface (an IP link over the cable).
    UsbNet,
    /// A pipe through the Android debug bridge.
    UsbAdb,
    /// A pipe through the Apple mobile device multiplexer.
    UsbMux,
    /// A pipe over the Android Open Accessory bulk endpoints.
    UsbAoa,
}

/// Where a session's peer is, for routing and display only. It is never
/// identity: certificate pins identify the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerLocator {
    Socket(SocketAddr),
    /// `device` is the provider's label for the device (a serial or similar).
    /// It is private to this machine: show it locally, never send or log it
    /// where it leaves the machine, and never base a trust decision on it.
    Usb {
        medium: Medium,
        device: String,
    },
}

/// A byte stream a provider opened to the peer: the TLS handshake and the
/// TLS/TCP framing run on it unchanged.
pub trait PipeStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> PipeStream for T {}
pub type PipeIo = Box<dyn PipeStream>;

/// A USB device as the admission bound counts it: a provider's label is only
/// unique within its medium.
type UsbDevice = (Medium, String);

/// How long a handed-over pipe may wait for the device's first byte.
const PIPE_IDLE_LIMIT: Duration = Duration::from_secs(60);
/// Idle pipes a listener holds in all.
const MAX_IDLE_PIPES: usize = 32;

/// How a pipe handed to [`SessionListener::accept_stream`] ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeOutcome {
    /// A session was negotiated on it and queued for `accept`.
    Accepted,
    /// The device never sent a byte within the idle limit. Normal.
    IdleTimeout,
    /// The pipe closed before the device sent a byte. Normal.
    ClosedIdle,
    /// The listener had no room for it (pipes per device, idle pipes, or
    /// handshakes in flight). Normal.
    Refused,
    /// The handshake or the hello failed, or the pipe broke.
    HandshakeFailed,
    /// The device's certificate is not one this listener trusts: retrying
    /// cannot help until trust changes or the device is replugged.
    Untrusted,
}

/// Resolves when a handed-over pipe has been accepted or given up.
pub struct PipeHandle(oneshot::Receiver<PipeOutcome>);

impl PipeHandle {
    /// The pipe's outcome. A listener that closed first reports
    /// [`PipeOutcome::Refused`].
    pub async fn outcome(self) -> PipeOutcome {
        self.0.await.unwrap_or(PipeOutcome::Refused)
    }
}

struct OfferedPipe {
    io: PipeIo,
    locator: PeerLocator,
    outcome: oneshot::Sender<PipeOutcome>,
}

/// A pipe whose first bytes were read to see that the device dialed: they are
/// replayed ahead of the rest.
struct Prefixed {
    prefix: Vec<u8>,
    taken: usize,
    inner: PipeIo,
}

impl AsyncRead for Prefixed {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.taken < self.prefix.len() {
            let count = buffer.remaining().min(self.prefix.len() - self.taken);
            let start = self.taken;
            buffer.put_slice(&self.prefix[start..start + count]);
            self.taken += count;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(context, data)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

/// Pending handshakes one USB device may hold at a time: a device that opens
/// pipes and stalls cannot use up the listener's shared handshake budget.
const MAX_PENDING_PER_DEVICE: usize = 2;

#[derive(Clone, Debug)]
pub struct DialPath {
    pub address: SocketAddr,
    pub kind: TransportKind,
    pub delay: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("connection failed")]
    Connection,
    #[error("handshake or negotiation timed out")]
    Timeout,
    #[error("session listener closed")]
    Closed,
    #[error("invalid path set")]
    InvalidPaths,
    #[error("no authenticated path succeeded")]
    NoPath,
    /// The listener's pending-handshake queue is full: the pipe was not taken,
    /// and the provider may offer one again later.
    #[error("too many pending handshakes")]
    Busy,
    #[error("protocol ALPN mismatch")]
    Alpn,
    #[error("network I/O failed")]
    Io(#[from] std::io::Error),
}

pub(crate) type LaneIo = (
    Box<dyn AsyncRead + Unpin + Send>,
    Box<dyn AsyncWrite + Unpin + Send>,
);

pub struct Session {
    pub(crate) lanes: Vec<LaneIo>,
    pub(crate) reader: Box<dyn AsyncRead + Unpin + Send>,
    pub(crate) writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub kind: TransportKind,
    pub peer_fingerprint: String,
    pub locator: PeerLocator,
    pub capabilities: Vec<String>,
    accepted_generation: Option<u64>,
    // Keep the endpoint alive for the lifetime of an outbound QUIC connection.
    pub(crate) endpoint: Option<quinn::Endpoint>,
    pub(crate) quic: Option<quinn::Connection>,
}

impl Session {
    pub async fn send(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        Ok(protocol::write_frame(&mut self.writer, payload).await?)
    }
    pub async fn receive(&mut self) -> Result<Vec<u8>, TransportError> {
        Ok(protocol::read_frame(&mut self.reader).await?)
    }
    /// A path change can rebind QUIC without changing the authenticated peer.
    /// TCP reconnect is driven by the owner with a fresh registry generation.
    pub fn rebind(&self, address: SocketAddr) -> Result<(), TransportError> {
        let endpoint = self.endpoint.as_ref().ok_or(TransportError::InvalidPaths)?;
        endpoint.rebind(std::net::UdpSocket::bind(address)?)?;
        Ok(())
    }
    async fn negotiate(mut self, hello: &Hello) -> Result<Self, TransportError> {
        self.capabilities =
            protocol::exchange_hello(&mut self.reader, &mut self.writer, hello).await?;
        if let Some(connection) = &self.quic {
            for lane in 1..crate::multiplex::LANES {
                let (mut writer, mut reader) = if self.endpoint.is_some() {
                    let (mut writer, reader) = connection
                        .open_bi()
                        .await
                        .map_err(|_| TransportError::Connection)?;
                    writer
                        .write_all(&[lane as u8])
                        .await
                        .map_err(|_| TransportError::Connection)?;
                    (writer, reader)
                } else {
                    connection
                        .accept_bi()
                        .await
                        .map_err(|_| TransportError::Connection)?
                };
                if self.endpoint.is_some() {
                    if reader.read_u8().await? != lane as u8 {
                        return Err(TransportError::Connection);
                    }
                } else {
                    if reader.read_u8().await? != lane as u8 {
                        return Err(TransportError::Connection);
                    }
                    writer
                        .write_all(&[lane as u8])
                        .await
                        .map_err(|_| TransportError::Connection)?;
                }
                writer
                    .set_priority(if lane == 2 || lane == 3 { 1 } else { 0 })
                    .map_err(|_| TransportError::Connection)?;
                self.lanes.push((Box::new(reader), Box::new(writer)));
            }
        }
        Ok(self)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(connection) = &self.quic {
            connection.close(0_u32.into(), b"session closed");
        }
    }
}

struct ListenerTrust {
    config: Arc<rustls::ServerConfig>,
    peers: BTreeSet<String>,
    generation: u64,
}

pub struct SessionListener {
    address: SocketAddr,
    tuning: TransportTuning,
    trust: Arc<StdMutex<ListenerTrust>>,
    quic: quinn::Endpoint,
    accepted: Mutex<mpsc::Receiver<Result<Session, TransportError>>>,
    pipes: mpsc::Sender<OfferedPipe>,
    accepting: JoinHandle<()>,
}

impl SessionListener {
    /// Binds with the process's [`transport_tuning`].
    pub async fn bind(
        address: SocketAddr,
        identity: &PeerIdentity,
        trusted: Vec<Vec<u8>>,
        hello: Hello,
    ) -> Result<Self, TransportError> {
        Self::bind_tuned(address, identity, trusted, hello, transport_tuning()).await
    }

    /// Binds with explicit transport tuning, used for every accepted connection.
    pub async fn bind_tuned(
        address: SocketAddr,
        identity: &PeerIdentity,
        trusted: Vec<Vec<u8>>,
        hello: Hello,
        tuning: TransportTuning,
    ) -> Result<Self, TransportError> {
        hello.negotiate(&hello)?;
        let peers = trusted
            .iter()
            .map(|cert| certificate_fingerprint(cert))
            .collect();
        let config = identity.server_config(trusted)?;
        let tcp = TcpListener::bind(address).await?;
        let (tcp, quic) =
            bind_both_transports(address, tcp, quic_server(&config, &tuning)?).await?;
        let address = tcp.local_addr()?;
        let trust = Arc::new(StdMutex::new(ListenerTrust {
            config,
            peers,
            generation: 1,
        }));
        let (sender, receiver) = mpsc::channel(MAX_PENDING_HANDSHAKES);
        let (pipes, pipe_receiver) = mpsc::channel(MAX_PENDING_HANDSHAKES);
        let accepting = tokio::spawn(accept_connections(
            tcp,
            quic.clone(),
            pipe_receiver,
            trust.clone(),
            hello,
            tuning,
            sender,
        ));
        Ok(Self {
            address,
            tuning,
            trust,
            quic,
            accepted: Mutex::new(receiver),
            pipes,
            accepting,
        })
    }

    /// Hands over a pipe a provider opened to a device. The listener is the
    /// TLS server on it, with the same identity, pins, admission bounds and
    /// trust generation as a TCP connection, and the session arrives through
    /// [`Self::accept`].
    ///
    /// The pipe may be handed over before the device dials: it waits idle for
    /// the device's first byte (60 s by default, `TransportTuning`), outside
    /// the handshake budget, and the handshake deadline starts at that byte.
    /// The returned handle reports how the pipe ended up, so a provider knows
    /// whether to open another. With the listener's pipe queue full the pipe is
    /// refused with [`TransportError::Busy`]; a third pipe from one device is
    /// [`PipeOutcome::Refused`].
    pub fn accept_stream(
        &self,
        io: PipeIo,
        locator: PeerLocator,
    ) -> Result<PipeHandle, TransportError> {
        if !matches!(locator, PeerLocator::Usb { .. }) {
            return Err(TransportError::InvalidPaths);
        }
        let (outcome, handle) = oneshot::channel();
        self.pipes
            .try_send(OfferedPipe {
                io,
                locator,
                outcome,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TransportError::Busy,
                mpsc::error::TrySendError::Closed(_) => TransportError::Closed,
            })?;
        Ok(PipeHandle(handle))
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.address)
    }

    /// Update listener configuration without rebinding the port or closing
    /// established QUIC connections. SecureListener owns per-peer revocation.
    /// A malformed replacement leaves the previous configuration untouched.
    pub(crate) fn replace_trust(
        &self,
        identity: &PeerIdentity,
        peers: Vec<Vec<u8>>,
    ) -> Result<(), TransportError> {
        let pins = peers
            .iter()
            .map(|cert| certificate_fingerprint(cert))
            .collect();
        let config = identity.server_config(peers)?;
        let server = quic_server(&config, &self.tuning)?;
        let mut trust = self.trust.lock().map_err(|_| TransportError::Closed)?;
        if trust.peers == pins {
            return Ok(());
        }
        let generation = trust
            .generation
            .checked_add(1)
            .ok_or(TransportError::Closed)?;
        self.quic.set_server_config(Some(server));
        *trust = ListenerTrust {
            config,
            peers: pins,
            generation,
        };
        Ok(())
    }

    pub async fn accept(&self) -> Result<Session, TransportError> {
        let session = self
            .accepted
            .lock()
            .await
            .recv()
            .await
            .ok_or(TransportError::Closed)??;
        if !self.is_current(&session)? {
            return Err(TransportError::Connection);
        }
        Ok(session)
    }

    pub(crate) fn is_current(&self, session: &Session) -> Result<bool, TransportError> {
        let trust = self.trust.lock().map_err(|_| TransportError::Closed)?;
        Ok(session.accepted_generation == Some(trust.generation)
            && trust.peers.contains(&session.peer_fingerprint))
    }
}

/// TCP and UDP have independent ephemeral-port allocators. A TCP-selected port
/// can already belong to a QUIC client/another UDP service. Retry only when the
/// caller requested any free port; an explicitly configured port must not move.
async fn bind_both_transports(
    requested: SocketAddr,
    mut tcp: TcpListener,
    config: quinn::ServerConfig,
) -> Result<(TcpListener, quinn::Endpoint), TransportError> {
    for attempt in 0..16 {
        match quinn::Endpoint::server(config.clone(), tcp.local_addr()?) {
            Ok(quic) => return Ok((tcp, quic)),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse
                    && requested.port() == 0
                    && attempt < 15 =>
            {
                // Retain the old TCP reservation while choosing another port.
                tcp = TcpListener::bind(requested).await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("last binding attempt returns its error")
}

fn quic_server(
    config: &rustls::ServerConfig,
    tuning: &TransportTuning,
) -> Result<quinn::ServerConfig, TransportError> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(config.clone())
        .map_err(|_| TransportError::Connection)?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server.transport_config(quic_transport(&tuning.quic, None));
    Ok(server)
}

enum Incoming {
    Tcp(TcpStream),
    Quic(Box<quinn::Incoming>),
    Pipe(PipeIo, PeerLocator),
}

/// A pipe waiting for the device's first byte, or a handshake in flight.
struct PipeTicket {
    device: UsbDevice,
    outcome: oneshot::Sender<PipeOutcome>,
}

impl PipeTicket {
    fn finish(self, per_device: &mut HashMap<UsbDevice, usize>, outcome: PipeOutcome) {
        if let Some(count) = per_device.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 {
                per_device.remove(&self.device);
            }
        }
        // Nobody may be waiting for the outcome.
        let _ = self.outcome.send(outcome);
    }
}

/// How a handshake that failed on a pipe is reported: a peer certificate this
/// listener does not trust is told apart, because retrying cannot help.
fn failed_outcome(error: &TransportError) -> PipeOutcome {
    let untrusted = match error {
        TransportError::Io(error) => error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
            .is_some_and(|tls| {
                matches!(
                    tls,
                    rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented
                )
            }),
        _ => false,
    };
    if untrusted {
        PipeOutcome::Untrusted
    } else {
        PipeOutcome::HandshakeFailed
    }
}

/// Waits for the device's first bytes on an idle pipe.
async fn first_bytes(mut io: PipeIo, limit: Duration) -> Result<PipeIo, PipeOutcome> {
    let mut prefix = vec![0u8; 1024];
    match tokio::time::timeout(limit, io.read(&mut prefix)).await {
        Err(_) => Err(PipeOutcome::IdleTimeout),
        Ok(Ok(0)) => Err(PipeOutcome::ClosedIdle),
        Ok(Err(_)) => Err(PipeOutcome::HandshakeFailed),
        Ok(Ok(read)) => {
            prefix.truncate(read);
            Ok(Box::new(Prefixed {
                prefix,
                taken: 0,
                inner: io,
            }))
        }
    }
}

// An unauthenticated peer that stalls must not hold up another user's pairing
// or reconnect. Bound both in-flight handshakes and completed results. Dropping
// the driver aborts every pending handshake through JoinSet ownership.
async fn accept_connections(
    tcp: TcpListener,
    quic: quinn::Endpoint,
    mut pipes: mpsc::Receiver<OfferedPipe>,
    trust: Arc<StdMutex<ListenerTrust>>,
    hello: Hello,
    tuning: TransportTuning,
    sender: mpsc::Sender<Result<Session, TransportError>>,
) {
    let mut pending: JoinSet<(Option<PipeTicket>, Result<Session, TransportError>)> =
        JoinSet::new();
    // Pipes that have not carried a byte yet. They are outside the handshake
    // budget: a provider opens them ahead of the device dialing.
    let mut idle: JoinSet<(PipeTicket, PeerLocator, Result<PipeIo, PipeOutcome>)> = JoinSet::new();
    // Idle pipes and handshakes per USB device.
    let mut per_device: HashMap<UsbDevice, usize> = HashMap::new();
    let idle_limit = tuning.pipe_idle_limit_ms.map_or(PIPE_IDLE_LIMIT, |ms| {
        Duration::from_millis(ms.max(1).into())
    });
    loop {
        let (incoming, ticket) = tokio::select! {
            _ = sender.closed() => break,
            completed = pending.join_next(), if !pending.is_empty() => {
                if let Some(completed) = completed {
                    let (ticket, result) =
                        completed.unwrap_or((None, Err(TransportError::Connection)));
                    if let Some(ticket) = ticket {
                        let outcome = match &result {
                            Ok(_) => PipeOutcome::Accepted,
                            Err(error) => failed_outcome(error),
                        };
                        ticket.finish(&mut per_device, outcome);
                    }
                    // A full result queue closes the excess session immediately.
                    // It cannot turn an absent native consumer into unbounded memory.
                    let _ = sender.try_send(result);
                }
                continue;
            }
            offered = pipes.recv() => {
                let Some(OfferedPipe { io, locator, outcome }) = offered else { break };
                let PeerLocator::Usb { medium, device } = &locator else { continue };
                let device = (*medium, device.clone());
                let count = per_device.entry(device.clone()).or_default();
                if *count >= MAX_PENDING_PER_DEVICE || idle.len() >= MAX_IDLE_PIPES {
                    if *count == 0 {
                        per_device.remove(&device);
                    }
                    // Dropping the pipe closes it.
                    let _ = outcome.send(PipeOutcome::Refused);
                    continue;
                }
                *count += 1;
                let ticket = PipeTicket { device, outcome };
                idle.spawn(async move {
                    let first = first_bytes(io, idle_limit).await;
                    (ticket, locator, first)
                });
                continue;
            }
            spoke = idle.join_next(), if !idle.is_empty() => {
                let Some(Ok((ticket, locator, first))) = spoke else { continue };
                match first {
                    // The device dialed: the handshake deadline starts now.
                    Ok(io) if pending.len() < MAX_PENDING_HANDSHAKES => {
                        (Incoming::Pipe(io, locator), Some(ticket))
                    }
                    Ok(_) => {
                        ticket.finish(&mut per_device, PipeOutcome::Refused);
                        continue;
                    }
                    Err(outcome) => {
                        ticket.finish(&mut per_device, outcome);
                        continue;
                    }
                }
            }
            stream = tcp.accept(), if pending.len() < MAX_PENDING_HANDSHAKES => {
                match stream {
                    Ok((stream, _)) => (Incoming::Tcp(stream), None),
                    Err(error) => { let _ = sender.try_send(Err(error.into())); break; }
                }
            }
            connection = quic.accept(), if pending.len() < MAX_PENDING_HANDSHAKES => {
                match connection {
                    Some(connection) => (Incoming::Quic(Box::new(connection)), None),
                    None => break,
                }
            }
        };
        let (tls, generation) = match trust.lock() {
            Ok(trust) => (
                tokio_rustls::TlsAcceptor::from(trust.config.clone()),
                trust.generation,
            ),
            Err(_) => break,
        };
        let hello = hello.clone();
        pending.spawn(async move {
            let session =
                accept_connection(incoming, tls, hello, tuning)
                    .await
                    .map(|mut session| {
                        session.accepted_generation = Some(generation);
                        session
                    });
            (ticket, session)
        });
    }
}

async fn accept_connection(
    incoming: Incoming,
    tls: tokio_rustls::TlsAcceptor,
    hello: Hello,
    tuning: TransportTuning,
) -> Result<Session, TransportError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let session = match incoming {
            Incoming::Tcp(stream) => {
                let locator = PeerLocator::Socket(stream.peer_addr()?);
                configure_tcp(&stream, &tuning)?;
                tls_server_session(tls, stream, TransportKind::TlsTcp, locator).await?
            }
            Incoming::Pipe(io, locator) => {
                let PeerLocator::Usb { medium, .. } = &locator else {
                    return Err(TransportError::InvalidPaths);
                };
                let kind = TransportKind::TlsStream { medium: *medium };
                tls_server_session(tls, io, kind, locator).await?
            }
            Incoming::Quic(incoming) => {
                let connection = (*incoming).await.map_err(|_| TransportError::Connection)?;
                let peer_address = connection.remote_address();
                let peer_fingerprint = quic_peer(&connection)?;
                let (writer, reader) = connection
                    .accept_bi()
                    .await
                    .map_err(|_| TransportError::Connection)?;
                writer
                    .set_priority(2)
                    .map_err(|_| TransportError::Connection)?;
                Session {
                    lanes: vec![],
                    reader: Box::new(reader),
                    writer: Box::new(writer),
                    kind: TransportKind::Quic,
                    peer_fingerprint,
                    locator: PeerLocator::Socket(peer_address),
                    capabilities: vec![],
                    accepted_generation: None,
                    endpoint: None,
                    quic: Some(connection),
                }
            }
        };
        session.negotiate(&hello).await
    })
    .await
    .map_err(|_| TransportError::Timeout)?
}

/// The server side of a TLS stream session: handshake, ALPN and the peer's pin.
async fn tls_server_session<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    tls: tokio_rustls::TlsAcceptor,
    stream: S,
    kind: TransportKind,
    locator: PeerLocator,
) -> Result<Session, TransportError> {
    let tls = tls.accept(stream).await?;
    let (_, state) = tls.get_ref();
    if state.alpn_protocol() != Some(SESSION_ALPN) {
        return Err(TransportError::Alpn);
    }
    let peer = state
        .peer_certificates()
        .and_then(|p| p.first())
        .ok_or(TransportError::Connection)?;
    let peer_fingerprint = certificate_fingerprint(peer);
    let (reader, writer) = tokio::io::split(tls);
    Ok(Session {
        lanes: vec![],
        reader: Box::new(reader),
        writer: Box::new(writer),
        kind,
        peer_fingerprint,
        locator,
        capabilities: vec![],
        accepted_generation: None,
        endpoint: None,
        quic: None,
    })
}

/// The client side of a TLS stream session.
async fn tls_client_session<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    config: Arc<rustls::ClientConfig>,
    stream: S,
    kind: TransportKind,
    locator: PeerLocator,
) -> Result<Session, TransportError> {
    let name = rustls::pki_types::ServerName::try_from(PEER_DNS_NAME)
        .map_err(|_| TransportError::Connection)?;
    let tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, stream)
        .await?;
    let (_, state) = tls.get_ref();
    if state.alpn_protocol() != Some(SESSION_ALPN) {
        return Err(TransportError::Alpn);
    }
    let peer = state
        .peer_certificates()
        .and_then(|p| p.first())
        .ok_or(TransportError::Connection)?;
    let peer_fingerprint = certificate_fingerprint(peer);
    let (reader, writer) = tokio::io::split(tls);
    Ok(Session {
        lanes: vec![],
        reader: Box::new(reader),
        writer: Box::new(writer),
        kind,
        peer_fingerprint,
        locator,
        capabilities: vec![],
        accepted_generation: None,
        endpoint: None,
        quic: None,
    })
}

/// Dials over a pipe the device side accepted from a provider (the host is
/// always the TLS server). One attempt within the handshake timeout and no
/// retry: a pipe that fails is gone, and the provider opens another.
pub async fn dial_pipe(
    identity: &PeerIdentity,
    trusted_peer: Vec<u8>,
    io: PipeIo,
    locator: PeerLocator,
    hello: Hello,
) -> Result<Session, TransportError> {
    let PeerLocator::Usb { medium, .. } = &locator else {
        return Err(TransportError::InvalidPaths);
    };
    let kind = TransportKind::TlsStream { medium: *medium };
    hello.negotiate(&hello)?;
    let config = identity.client_config(trusted_peer)?;
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        tls_client_session(config, io, kind, locator)
            .await?
            .negotiate(&hello)
            .await
    })
    .await
    .map_err(|_| TransportError::Timeout)?
}

impl Drop for SessionListener {
    fn drop(&mut self) {
        self.accepting.abort();
        self.quic.close(0_u32.into(), b"listener closed");
    }
}

/// Dials with the process's [`transport_tuning`].
pub async fn dial(
    identity: &PeerIdentity,
    trusted_peer: Vec<u8>,
    paths: Vec<DialPath>,
    hello: Hello,
) -> Result<Session, TransportError> {
    dial_tuned(identity, trusted_peer, paths, hello, transport_tuning()).await
}

/// Dials with explicit transport tuning.
pub async fn dial_tuned(
    identity: &PeerIdentity,
    trusted_peer: Vec<u8>,
    paths: Vec<DialPath>,
    hello: Hello,
    tuning: TransportTuning,
) -> Result<Session, TransportError> {
    if paths.is_empty()
        || paths.len() > MAX_PATHS
        || paths.iter().any(|p| {
            p.delay > Duration::from_secs(2)
                || matches!(p.kind, TransportKind::TlsStream { .. })
                || p.address.port() == 0
                || p.address.ip().is_unspecified()
                || p.address.ip().is_multicast()
        })
    {
        return Err(TransportError::InvalidPaths);
    }
    hello.negotiate(&hello)?;
    let config = identity.client_config(trusted_peer)?;
    let mut pending = JoinSet::new();
    for path in paths {
        let config = config.clone();
        let hello = hello.clone();
        pending.spawn(async move {
            tokio::time::sleep(path.delay).await;
            tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                connect_path(config, path, &hello, &tuning),
            )
            .await
            .map_err(|_| TransportError::Timeout)?
        });
    }
    while let Some(result) = pending.join_next().await {
        if let Ok(Ok(session)) = result {
            // JoinSet Drop aborts and drops every loser, including its socket.
            // Nothing beyond Hello has been sent on those paths.
            return Ok(session);
        }
    }
    Err(TransportError::NoPath)
}

async fn connect_path(
    config: Arc<rustls::ClientConfig>,
    path: DialPath,
    hello: &Hello,
    tuning: &TransportTuning,
) -> Result<Session, TransportError> {
    let mut retry_delay = Duration::from_millis(150);
    loop {
        let result = connect_path_once(config.clone(), path.clone(), hello, tuning).await;
        // A short outage can reject TCP before a black-holed QUIC attempt
        // finishes. Keep that TCP candidate eligible within the same five-second
        // deadline, rather than waiting for every other candidate to time out.
        // Identity, ALPN and protocol errors are never retried here. Application
        // data is sent only by the caller after a negotiated winner is returned.
        if path.kind == TransportKind::TlsTcp
            && matches!(&result, Err(TransportError::Io(error)) if matches!(error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut))
        {
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(Duration::from_millis(500));
            continue;
        }
        return result;
    }
}

async fn connect_path_once(
    config: Arc<rustls::ClientConfig>,
    path: DialPath,
    hello: &Hello,
    tuning: &TransportTuning,
) -> Result<Session, TransportError> {
    let session = match path.kind {
        TransportKind::TlsTcp => {
            let stream = TcpStream::connect(path.address).await?;
            configure_tcp(&stream, tuning)?;
            let locator = PeerLocator::Socket(path.address);
            tls_client_session(config, stream, TransportKind::TlsTcp, locator).await?
        }
        // A pipe is dialed with `dial_pipe`, never as a socket path.
        TransportKind::TlsStream { .. } => return Err(TransportError::InvalidPaths),
        TransportKind::Quic => {
            let bind = if path.address.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            let mut endpoint =
                quinn::Endpoint::client(bind.parse().map_err(|_| TransportError::InvalidPaths)?)?;
            let crypto = quinn::crypto::rustls::QuicClientConfig::try_from((*config).clone())
                .map_err(|_| TransportError::Connection)?;
            let mut client = quinn::ClientConfig::new(Arc::new(crypto));
            client.transport_config(quic_transport(&tuning.quic, Some(path.address)));
            endpoint.set_default_client_config(client);
            let connection = endpoint
                .connect(path.address, PEER_DNS_NAME)
                .map_err(|_| TransportError::Connection)?
                .await
                .map_err(|_| TransportError::Connection)?;
            let peer_fingerprint = quic_peer(&connection)?;
            let (writer, reader) = connection
                .open_bi()
                .await
                .map_err(|_| TransportError::Connection)?;
            writer
                .set_priority(2)
                .map_err(|_| TransportError::Connection)?;
            Session {
                lanes: vec![],
                reader: Box::new(reader),
                writer: Box::new(writer),
                kind: TransportKind::Quic,
                peer_fingerprint,
                locator: PeerLocator::Socket(path.address),
                capabilities: vec![],
                accepted_generation: None,
                endpoint: Some(endpoint),
                quic: Some(connection),
            }
        }
    };
    session.negotiate(hello).await
}

/// No Nagle delay on every TLS/TCP socket; the unsent-byte bound when tuned.
fn configure_tcp(stream: &TcpStream, tuning: &TransportTuning) -> Result<(), TransportError> {
    stream.set_nodelay(true)?;
    if let Some(bytes) = tuning.tcp_notsent_lowat {
        crate::sockopt::set_notsent_lowat(stream, bytes)?;
    }
    Ok(())
}

fn quic_peer(connection: &quinn::Connection) -> Result<String, TransportError> {
    let handshake = connection
        .handshake_data()
        .ok_or(TransportError::Connection)?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .map_err(|_| TransportError::Connection)?;
    if handshake.protocol.as_deref() != Some(SESSION_ALPN) {
        return Err(TransportError::Alpn);
    }
    let identity = connection
        .peer_identity()
        .ok_or(TransportError::Connection)?
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| TransportError::Connection)?;
    let certificate = identity.first().ok_or(TransportError::Connection)?;
    Ok(certificate_fingerprint(certificate))
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    // A TCP allocation alone does not reserve its independently allocated UDP
    // port. Reserve both before intentionally making QUIC collide with our UDP
    // blocker; unrelated UDP users must not cause a fixture-construction panic.
    async fn collision_fixture() -> (TcpListener, std::net::UdpSocket) {
        for _ in 0..16 {
            let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
            match std::net::UdpSocket::bind(tcp.local_addr().unwrap()) {
                Ok(udp) => return (tcp, udp),
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(error) => panic!("cannot reserve UDP collision fixture: {error}"),
            }
        }
        panic!("cannot reserve a shared TCP/UDP collision fixture port");
    }

    #[tokio::test]
    async fn udp_collision_reselects_only_an_ephemeral_port() {
        for ephemeral in [true, false] {
            let identity = PeerIdentity::generate().unwrap();
            let config = quic_server(
                &identity.server_config(Vec::new()).unwrap(),
                &TransportTuning::default(),
            )
            .unwrap();
            let (tcp, udp) = collision_fixture().await;
            let occupied = tcp.local_addr().unwrap();
            let mut requested = occupied;
            if ephemeral {
                requested.set_port(0);
            }
            match bind_both_transports(requested, tcp, config).await {
                Ok((tcp, quic)) => {
                    assert!(ephemeral, "a configured port must not silently change");
                    assert_ne!(tcp.local_addr().unwrap().port(), occupied.port());
                    assert_eq!(tcp.local_addr().unwrap(), quic.local_addr().unwrap());
                }
                Err(TransportError::Io(error)) if !ephemeral => {
                    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
                }
                _ => panic!("unexpected shared-port binding result"),
            }
            assert_eq!(udp.local_addr().unwrap(), occupied);
        }
    }
}
