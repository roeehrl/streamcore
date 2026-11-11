//! Async native binding surface. A shared Rust runtime owns network I/O; Swift
//! and Kotlin await results without blocking their UI executors. Cancellation
//! aborts the Rust task and closes any partially consumed/written frame.

use crate::{
    clock::{CLOCK_CAPABILITY, ClockEstimate, monotonic_now_us},
    identity::{PeerIdentity, certificate_fingerprint},
    multiplex::{ChannelLane, LanePolicy, Multiplexer, PendingSend, binary_lanes},
    protocol::{self, Hello},
    transport::{self, DialPath, Session, SessionListener, TransportKind},
};
use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ChannelError {
    #[error("invalid address, trust set, or negotiation configuration")]
    InvalidConfiguration,
    #[error("authenticated connection failed")]
    ConnectionFailed,
    #[error("channel is closed")]
    Closed,
    #[error("network operation failed; reconnect before retrying")]
    Io,
    #[error("the bounded send queue is full")]
    Backpressure,
    #[error("network runtime is unavailable")]
    Runtime,
}

#[derive(Clone, Copy, uniffi::Enum)]
pub enum ChannelTransport {
    Quic,
    TlsTcp,
}

#[derive(Clone, uniffi::Record)]
pub struct ChannelPath {
    /// Numeric IPv4 or bracketed IPv6 socket address, already resolved by the
    /// native discovery layer. No caller-controlled TLS server name is used.
    pub address: String,
    pub transport: ChannelTransport,
    pub delay_ms: u32,
}

#[derive(Clone, uniffi::Record)]
pub struct SessionOptions {
    pub capabilities: Vec<String>,
    pub required: Vec<String>,
}

impl SessionOptions {
    fn hello(self) -> Hello {
        Hello {
            major: protocol::PROTOCOL_MAJOR,
            capabilities: self.capabilities,
            required: self.required,
        }
    }
}

async fn on_runtime<T: Send + 'static>(
    future: impl Future<Output = Result<T, ChannelError>> + Send + 'static,
) -> Result<T, ChannelError> {
    crate::runtime::run(future)
        .await
        .map_err(|_| ChannelError::Runtime)?
}

/// A received message with its arrival times (`clock::monotonic_now_us`).
#[derive(Clone, uniffi::Record)]
pub struct TimedMessage {
    pub payload: Vec<u8>,
    /// When its first fragment arrived.
    pub first_byte_us: u64,
    /// When its last fragment arrived and it became complete.
    pub complete_us: u64,
}

#[derive(uniffi::Object)]
pub struct SecureChannel {
    io: Arc<Multiplexer>,
    transport: ChannelTransport,
    peer: String,
    peer_address: std::net::SocketAddr,
    capabilities: Vec<String>,
    lanes: LanePolicy,
}

impl Drop for SecureChannel {
    fn drop(&mut self) {
        self.io.close();
    }
}

impl SecureChannel {
    fn from_session(mut session: Session, lanes: LanePolicy) -> Arc<Self> {
        let clock = session.capabilities.iter().any(|c| c == CLOCK_CAPABILITY);
        let io = Multiplexer::with_clock(session.quic.take(), session.endpoint.take(), clock);
        let reader = std::mem::replace(&mut session.reader, Box::new(tokio::io::empty()));
        let writer = std::mem::replace(&mut session.writer, Box::new(tokio::io::sink()));
        let quic = session.kind == TransportKind::Quic;
        io.start(reader, writer, if quic { Some(0) } else { None });
        for (index, (reader, writer)) in session.lanes.drain(..).enumerate() {
            io.start(reader, writer, Some(index + 1));
        }
        Arc::new(Self {
            transport: if quic {
                ChannelTransport::Quic
            } else {
                ChannelTransport::TlsTcp
            },
            peer: session.peer_fingerprint.clone(),
            peer_address: session.peer_address,
            capabilities: session.capabilities.clone(),
            lanes,
            io,
        })
    }
}

struct CloseOnInterruptedFrame {
    io: Arc<Multiplexer>,
    completed: bool,
}
impl Drop for CloseOnInterruptedFrame {
    fn drop(&mut self) {
        if !self.completed {
            self.io.close();
        }
    }
}

#[uniffi::export]
impl SecureChannel {
    pub fn transport(&self) -> ChannelTransport {
        self.transport
    }
    pub fn peer_fingerprint(&self) -> String {
        self.peer.clone()
    }
    /// Routing/display information only. Certificate pins, never addresses,
    /// identify the peer. QUIC reports its current path after migration.
    pub fn remote_address(&self) -> String {
        self.io
            .quic
            .as_ref()
            .map(quinn::Connection::remote_address)
            .unwrap_or(self.peer_address)
            .to_string()
    }
    pub fn capabilities(&self) -> Vec<String> {
        self.capabilities.clone()
    }
    pub fn close(&self) {
        self.io.close();
    }

    /// Synchronous admission preserves native caller order without racing Tasks.
    pub fn enqueue(
        &self,
        lane: ChannelLane,
        payload: Vec<u8>,
    ) -> Result<Arc<PendingSend>, ChannelError> {
        self.io.enqueue(lane, payload)
    }

    /// Under `clock.v1` the core alone sends pings and pongs; an application
    /// frame of either kind is refused.
    pub fn enqueue_application_frame(
        &self,
        payload: Vec<u8>,
    ) -> Result<Arc<PendingSend>, ChannelError> {
        let lane = crate::multiplex::application_lane(&payload, self.lanes)?;
        if self.io.clock.is_some() && matches!(payload.first(), Some(2 | 3)) {
            return Err(ChannelError::InvalidConfiguration);
        }
        self.io.enqueue(lane, payload)
    }

    /// Sends a typed ping on lane 0 (`clock.v1` only). The peer's core answers
    /// it; the answer updates [`Self::clock_estimate`] and is also delivered to
    /// the application as a pong frame, for liveness.
    pub fn send_clock_ping(&self) -> Result<(), ChannelError> {
        self.io.ping()
    }

    /// The peer clock relative to ours, once a typed ping has been answered.
    pub fn clock_estimate(&self) -> Option<ClockEstimate> {
        self.io
            .clock
            .as_ref()
            .and_then(|clock| clock.estimate(monotonic_now_us()))
    }

    /// Convenience for generic control-lane probes. Applications enqueue complete
    /// typed frames so the core can preserve each protocol dependency's lane.
    pub async fn send(&self, payload: Vec<u8>) -> Result<(), ChannelError> {
        let io = self.io.clone();
        on_runtime(async move {
            let mut guard = CloseOnInterruptedFrame {
                io: io.clone(),
                completed: false,
            };
            io.enqueue(ChannelLane::Interactive, payload)?
                .written()
                .await?;
            guard.completed = true;
            Ok(())
        })
        .await
    }

    /// [`Self::receive`] plus when the message's first and last fragments
    /// arrived, for receive-side stage timing.
    pub async fn receive_timed(&self) -> Result<TimedMessage, ChannelError> {
        let io = self.io.clone();
        on_runtime(async move {
            let mut guard = CloseOnInterruptedFrame {
                io: io.clone(),
                completed: false,
            };
            let received = io.receive_timed().await?;
            guard.completed = true;
            Ok(TimedMessage {
                payload: received.data,
                first_byte_us: received.first_us,
                complete_us: received.complete_us,
            })
        })
        .await
    }

    pub async fn receive(&self) -> Result<Vec<u8>, ChannelError> {
        let io = self.io.clone();
        on_runtime(async move {
            let mut guard = CloseOnInterruptedFrame {
                io: io.clone(),
                completed: false,
            };
            let bytes = io.receive().await?;
            guard.completed = true;
            Ok(bytes)
        })
        .await
    }

    pub async fn rebind(&self, address: String) -> Result<(), ChannelError> {
        let io = self.io.clone();
        on_runtime(async move {
            if *io.closed.borrow() {
                return Err(ChannelError::Closed);
            }
            let address: std::net::SocketAddr = address
                .parse()
                .map_err(|_| ChannelError::InvalidConfiguration)?;
            io.endpoint
                .as_ref()
                .ok_or(ChannelError::InvalidConfiguration)?
                .rebind(std::net::UdpSocket::bind(address).map_err(|_| ChannelError::Io)?)
                .map_err(|_| ChannelError::Io)
        })
        .await
    }
}

struct ListenerState {
    listener: Option<Arc<SessionListener>>,
    channels: Vec<Weak<SecureChannel>>,
}

#[derive(uniffi::Object)]
pub struct SecureListener {
    state: Mutex<ListenerState>,
    identity: Arc<PeerIdentity>,
    address: String,
    closed: watch::Sender<bool>,
    lanes: LanePolicy,
}

impl Drop for SecureListener {
    fn drop(&mut self) {
        self.close();
    }
}

#[uniffi::export]
impl SecureListener {
    pub fn address(&self) -> String {
        self.address.clone()
    }

    pub fn close(&self) {
        self.closed.send_replace(true);
        if let Ok(mut state) = self.state.lock() {
            state.listener.take();
            for channel in state.channels.drain(..).filter_map(|weak| weak.upgrade()) {
                channel.close();
            }
        }
    }

    /// Apply only after native credential persistence succeeds. Additions leave
    /// existing channels alive; removed pins close every retained channel for
    /// that peer. Empty trust denies all connections while pairing remains open.
    pub fn replace_trusted_peers(&self, trusted_peers: Vec<Vec<u8>>) -> Result<(), ChannelError> {
        let pins: BTreeSet<String> = trusted_peers
            .iter()
            .map(|cert| certificate_fingerprint(cert))
            .collect();
        let mut state = self.state.lock().map_err(|_| ChannelError::Closed)?;
        let listener = state.listener.as_ref().ok_or(ChannelError::Closed)?;
        listener
            .replace_trust(&self.identity, trusted_peers)
            .map_err(|_| ChannelError::InvalidConfiguration)?;
        state.channels.retain(|weak| {
            let Some(channel) = weak.upgrade() else {
                return false;
            };
            if !pins.contains(&channel.peer) {
                channel.close();
            }
            !*channel.io.closed.borrow()
        });
        Ok(())
    }

    pub async fn accept(&self) -> Result<Arc<SecureChannel>, ChannelError> {
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return Err(ChannelError::Closed);
        }
        let listener = self
            .state
            .lock()
            .map_err(|_| ChannelError::Closed)?
            .listener
            .clone()
            .ok_or(ChannelError::Closed)?;
        let accepting = listener.clone();
        let session = on_runtime(async move {
            tokio::select! {
                _ = closed.changed() => Err(ChannelError::Closed),
                session = accepting.accept() => session.map_err(|_| ChannelError::ConnectionFailed),
            }
        })
        .await?;
        // Serialize delivery with trust replacement and close. A handshake
        // completed before revocation must not surface after the update returns.
        let mut state = self.state.lock().map_err(|_| ChannelError::Closed)?;
        if state.listener.is_none() {
            return Err(ChannelError::Closed);
        }
        if !listener
            .is_current(&session)
            .map_err(|_| ChannelError::Closed)?
        {
            return Err(ChannelError::ConnectionFailed);
        }
        state.channels.retain(|weak| {
            weak.upgrade()
                .is_some_and(|channel| !*channel.io.closed.borrow())
        });
        if state.channels.len() >= 256 {
            return Err(ChannelError::ConnectionFailed);
        }
        let channel = SecureChannel::from_session(session, self.lanes);
        state.channels.push(Arc::downgrade(&channel));
        Ok(channel)
    }
}

/// Listens with StreamCore's binary lanes only. Applications whose control
/// messages pick a lane listen with [`listen_secure_with`].
pub async fn listen_secure(
    address: String,
    identity: Arc<PeerIdentity>,
    trusted_peers: Vec<Vec<u8>>,
    options: SessionOptions,
) -> Result<Arc<SecureListener>, ChannelError> {
    listen_secure_with(binary_lanes, address, identity, trusted_peers, options).await
}

/// Listens; every accepted channel routes application frames with `lanes`.
pub async fn listen_secure_with(
    lanes: LanePolicy,
    address: String,
    identity: Arc<PeerIdentity>,
    trusted_peers: Vec<Vec<u8>>,
    options: SessionOptions,
) -> Result<Arc<SecureListener>, ChannelError> {
    on_runtime(async move {
        let listener = SessionListener::bind(
            address
                .parse()
                .map_err(|_| ChannelError::InvalidConfiguration)?,
            &identity,
            trusted_peers,
            options.hello(),
        )
        .await
        .map_err(|_| ChannelError::ConnectionFailed)?;
        let address = listener
            .local_addr()
            .map_err(|_| ChannelError::Io)?
            .to_string();
        Ok(Arc::new(SecureListener {
            state: Mutex::new(ListenerState {
                listener: Some(Arc::new(listener)),
                channels: vec![],
            }),
            identity,
            address,
            closed: watch::channel(false).0,
            lanes,
        }))
    })
    .await
}

/// Connects with StreamCore's binary lanes only. Applications whose control
/// messages pick a lane connect with [`connect_secure_with`].
pub async fn connect_secure(
    identity: Arc<PeerIdentity>,
    trusted_peer: Vec<u8>,
    paths: Vec<ChannelPath>,
    options: SessionOptions,
    cancellation: Arc<crate::OperationCancellation>,
) -> Result<Arc<SecureChannel>, ChannelError> {
    connect_secure_with(
        binary_lanes,
        identity,
        trusted_peer,
        paths,
        options,
        cancellation,
    )
    .await
}

/// Connects; the channel routes application frames with `lanes`.
pub async fn connect_secure_with(
    lanes: LanePolicy,
    identity: Arc<PeerIdentity>,
    trusted_peer: Vec<u8>,
    paths: Vec<ChannelPath>,
    options: SessionOptions,
    cancellation: Arc<crate::OperationCancellation>,
) -> Result<Arc<SecureChannel>, ChannelError> {
    let paths = paths
        .into_iter()
        .map(|p| {
            Ok(DialPath {
                address: p
                    .address
                    .parse()
                    .map_err(|_| ChannelError::InvalidConfiguration)?,
                kind: match p.transport {
                    ChannelTransport::Quic => TransportKind::Quic,
                    ChannelTransport::TlsTcp => TransportKind::TlsTcp,
                },
                delay: Duration::from_millis(p.delay_ms.into()),
            })
        })
        .collect::<Result<Vec<_>, ChannelError>>()?;
    on_runtime(async move {
        cancellation
            .run(transport::dial(
                &identity,
                trusted_peer,
                paths,
                options.hello(),
            ))
            .await
            .ok_or(ChannelError::Closed)?
            .map(|session| SecureChannel::from_session(session, lanes))
            .map_err(|_| ChannelError::ConnectionFailed)
    })
    .await
}
