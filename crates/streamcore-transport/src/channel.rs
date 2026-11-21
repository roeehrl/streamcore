//! Async native binding surface. A shared Rust runtime owns network I/O; Swift
//! and Kotlin await results without blocking their UI executors. Cancellation
//! aborts the Rust task and closes any partially consumed/written frame.

use crate::{
    clock::{CLOCK_CAPABILITY, ClockEstimate, monotonic_now_us},
    identity::{PeerIdentity, certificate_fingerprint},
    multiplex::{
        ChannelLane, Features, LanePolicy, Multiplexer, PendingSend, binary_lanes,
        check_frames_version,
    },
    protocol::{self, Hello},
    transport::{
        self, DialPath, Medium, PeerLocator, PipeHandle, PipeIo, PipeOutcome, Session,
        SessionListener, TransportError, TransportKind,
    },
};
use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use streamcore_media::media::FRAMES_V4_CAPABILITY;
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
    /// A listener's pending-handshake queue is full: offer the pipe again later.
    #[error("too many pending handshakes")]
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChannelTransport {
    Quic,
    TlsTcp,
    /// The TLS/TCP framing over a pipe a USB provider opened.
    TlsStream {
        medium: Medium,
    },
}

/// Where a channel's peer is, for routing and display only; never identity.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChannelLocator {
    /// A numeric socket address.
    Socket { address: String },
    /// `device` is the provider's label for the device. It is private to this
    /// machine: show it locally, and never send or log it where it leaves the
    /// machine.
    Usb { medium: Medium, device: String },
}

/// A byte stream a USB provider opened between this machine and a device.
pub struct UsbPipe {
    pub io: PipeIo,
    pub medium: Medium,
    /// The provider's label for the device, for local display only.
    pub device: String,
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
    locator: PeerLocator,
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
        let has = |name: &str| session.capabilities.iter().any(|c| c == name);
        let features = Features {
            clock: has(CLOCK_CAPABILITY),
            frames_v4: has(FRAMES_V4_CAPABILITY),
            bulk_token_bucket: crate::tuning::transport_tuning().bulk_token_bucket,
        };
        let io = Multiplexer::with_features(session.quic.take(), session.endpoint.take(), features);
        let reader = std::mem::replace(&mut session.reader, Box::new(tokio::io::empty()));
        let writer = std::mem::replace(&mut session.writer, Box::new(tokio::io::sink()));
        let quic = session.kind == TransportKind::Quic;
        io.start(reader, writer, if quic { Some(0) } else { None });
        for (index, (reader, writer)) in session.lanes.drain(..).enumerate() {
            io.start(reader, writer, Some(index + 1));
        }
        // A relayed pipe reports no unplug: the multiplexer watches it itself.
        if matches!(
            session.kind,
            TransportKind::TlsStream {
                medium: Medium::UsbAdb | Medium::UsbMux | Medium::UsbAoa
            }
        ) {
            let tuning = crate::tuning::transport_tuning();
            let bound = |custom: Option<u32>, default: Duration| {
                custom.map_or(default, |ms| Duration::from_millis(ms.max(1).into()))
            };
            io.start_liveness(
                crate::multiplex::PIPE_PING_INTERVAL,
                bound(tuning.pipe_dead_after_ms, crate::multiplex::PIPE_DEAD_AFTER),
                bound(
                    tuning.pipe_pong_deadline_ms,
                    crate::multiplex::PIPE_PONG_DEADLINE,
                ),
            );
        }
        Arc::new(Self {
            transport: match session.kind {
                TransportKind::Quic => ChannelTransport::Quic,
                TransportKind::TlsTcp => ChannelTransport::TlsTcp,
                TransportKind::TlsStream { medium } => ChannelTransport::TlsStream { medium },
            },
            peer: session.peer_fingerprint.clone(),
            locator: session.locator.clone(),
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

/// What [`SecureChannel::link_stats`] reports. The QUIC fields are `None` or
/// zero on TLS/TCP. Counters are cumulative for the channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct LinkStats {
    /// quinn's smoothed RTT.
    pub rtt_us: Option<u64>,
    /// quinn's minimum RTT.
    pub min_rtt_us: Option<u64>,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub congestion_events: u64,
    pub cwnd_bytes: u64,
    /// Bulk-lane payload bytes handed to the writer.
    pub bulk_bytes_sent: u64,
}

/// The most pings one probe sends: half of what the clock remembers as
/// unanswered (`clock::OUTSTANDING`), so other pings on the channel at the same
/// time (an application's clock pings, another probe) cannot push a probe's own
/// out of that window and make an answered ping read as lost.
pub const MAX_PROBE_PINGS: u8 = 16;

/// What [`SecureChannel::probe`] measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ProbeResult {
    /// Pings sent (fewer than asked when the deadline came first).
    pub sent: u32,
    /// Pongs that arrived before the deadline.
    pub answered: u32,
    /// Round trips of the answered pings, excluding the peer's hold time.
    /// `None` when nothing was answered.
    pub rtt_p50_us: Option<u64>,
    pub rtt_p95_us: Option<u64>,
    /// How long the probe took.
    pub elapsed_ms: u32,
}

// A probe and as many other pings again fit the clock's window together.
const _: () = assert!(MAX_PROBE_PINGS as usize * 2 <= crate::clock::OUTSTANDING);

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
    /// Empty for a channel over a USB pipe, which has no address: see
    /// [`Self::locator`].
    pub fn remote_address(&self) -> String {
        match (&self.io.quic, &self.locator) {
            (Some(quic), _) => quic.remote_address().to_string(),
            (None, PeerLocator::Socket(address)) => address.to_string(),
            (None, PeerLocator::Usb { .. }) => String::new(),
        }
    }
    /// Where the peer is: its current socket address, or the USB medium and
    /// the provider's device label.
    pub fn locator(&self) -> ChannelLocator {
        match &self.locator {
            PeerLocator::Usb { medium, device } => ChannelLocator::Usb {
                medium: *medium,
                device: device.clone(),
            },
            PeerLocator::Socket(_) => ChannelLocator::Socket {
                address: self.remote_address(),
            },
        }
    }
    pub fn capabilities(&self) -> Vec<String> {
        self.capabilities.clone()
    }
    /// Both peers negotiated `frames.v4`: video frames may carry the extension
    /// block and the lane-0 video kinds `0x12`–`0x15` may be sent.
    pub fn frames_v4(&self) -> bool {
        self.io.features.frames_v4
    }
    /// Rate inputs for the bulk token bucket, in bits per second: the link
    /// capacity estimate and the current video rate. While video is active, bulk
    /// is limited to (capacity − video) × 0.8 (at least 1 Mbit/s). A zero
    /// capacity removes the inputs. They have no effect unless the process's
    /// `TransportTuning::bulk_token_bucket` was on when this channel was created.
    pub fn set_bulk_rate_inputs(
        &self,
        capacity_bps: u64,
        video_bps: u64,
    ) -> Result<(), ChannelError> {
        self.io.set_bulk_rates(capacity_bps, video_bps)
    }
    /// The bulk limit in bytes per second that applies while video is active, or
    /// None when the bucket is off or has no rate inputs.
    pub fn bulk_limit_bytes_per_second(&self) -> Option<u64> {
        self.io.bulk_limit()
    }
    /// Whether this channel runs the bulk token bucket: the process's
    /// `TransportTuning::bulk_token_bucket` was on when it was created. Without
    /// it, rate inputs are kept but limit nothing, and bulk can starve video
    /// (6.4 item 5).
    pub fn bulk_bucket_enabled(&self) -> bool {
        self.io.features.bulk_token_bucket
    }
    /// The channel is closed, by either side or by a transport failure.
    /// Nothing more can be sent on it. A leaf call.
    pub fn is_closed(&self) -> bool {
        *self.io.closed.borrow()
    }

    pub fn close(&self) {
        self.io.close();
    }

    /// The link's statistics for rate adaptation (STREAMCORE-VIDEO.md 6.6): on
    /// QUIC, quinn's path statistics; on TLS/TCP only the bulk byte count.
    pub fn link_stats(&self) -> LinkStats {
        let path = self.io.quic.as_ref().map(|quic| quic.stats().path);
        LinkStats {
            rtt_us: path.map(|p| u64::try_from(p.rtt.as_micros()).unwrap_or(u64::MAX)),
            min_rtt_us: path.map(|p| u64::try_from(p.min_rtt.as_micros()).unwrap_or(u64::MAX)),
            lost_packets: path.map_or(0, |p| p.lost_packets),
            sent_packets: path.map_or(0, |p| p.sent_packets),
            congestion_events: path.map_or(0, |p| p.congestion_events),
            cwnd_bytes: path.map_or(0, |p| p.cwnd),
            bulk_bytes_sent: self.io.bulk_bytes(),
        }
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
    /// frame of either kind is refused. Without `frames.v4` the lane-0 video kinds
    /// and video frames with the extension flag are refused; with it, the lane-0
    /// video bodies must be valid.
    pub fn enqueue_application_frame(
        &self,
        payload: Vec<u8>,
    ) -> Result<Arc<PendingSend>, ChannelError> {
        let lane = crate::multiplex::application_lane(&payload, self.lanes)?;
        if self.io.clock.is_some() && matches!(payload.first(), Some(2 | 3)) {
            return Err(ChannelError::InvalidConfiguration);
        }
        check_frames_version(&payload, self.io.features.frames_v4)?;
        self.io.enqueue(lane, payload)
    }

    /// Sends a typed ping on lane 0 (`clock.v1` only). The peer's core answers
    /// it; the answer updates [`Self::clock_estimate`] and is also delivered to
    /// the application as a pong frame, for liveness.
    pub fn send_clock_ping(&self) -> Result<(), ChannelError> {
        self.io.ping().map(|_| ())
    }

    /// Measures the link with `count` typed pings `spacing_ms` apart, waiting
    /// at most `deadline_ms` in all (`clock.v1` only): how a client judges a
    /// newly connected transport, a cable for example, before it moves a live
    /// session onto it. It sends nothing but the pings, so it may run before
    /// any application message. The caller decides what passes. At most
    /// [`MAX_PROBE_PINGS`] pings.
    pub async fn probe(
        &self,
        count: u8,
        spacing_ms: u32,
        deadline_ms: u32,
    ) -> Result<ProbeResult, ChannelError> {
        if count == 0 || count > MAX_PROBE_PINGS || deadline_ms == 0 || deadline_ms > 10_000 {
            return Err(ChannelError::InvalidConfiguration);
        }
        if self.io.clock.is_none() {
            return Err(ChannelError::InvalidConfiguration);
        }
        let io = self.io.clone();
        on_runtime(async move {
            let started = tokio::time::Instant::now();
            let deadline = started + Duration::from_millis(deadline_ms.into());
            let spacing = Duration::from_millis(spacing_ms.into());
            let mut nonces = Vec::with_capacity(count.into());
            for index in 0..u32::from(count) {
                let at = started + spacing * index;
                if at >= deadline {
                    break;
                }
                tokio::time::sleep_until(at).await;
                nonces.push(io.ping()?);
            }
            let round_trips = |io: &Multiplexer| {
                io.clock
                    .as_ref()
                    .map(|clock| clock.round_trips(&nonces))
                    .unwrap_or_default()
            };
            // Every pong, the deadline, or the channel closing ends the wait.
            let mut closed = io.closed.subscribe();
            let mut answered = round_trips(&io);
            while answered.len() < nonces.len()
                && tokio::time::Instant::now() < deadline
                && !*closed.borrow()
            {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                    _ = closed.changed() => {}
                }
                answered = round_trips(&io);
            }
            answered.sort_unstable();
            let percentile = |per_hundred: usize| {
                answered
                    .get((answered.len() * per_hundred).div_ceil(100).max(1) - 1)
                    .copied()
            };
            Ok(ProbeResult {
                sent: u32::try_from(nonces.len()).unwrap_or(u32::MAX),
                answered: u32::try_from(answered.len()).unwrap_or(u32::MAX),
                rtt_p50_us: percentile(50),
                rtt_p95_us: percentile(95),
                elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
            })
        })
        .await
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

/// How a pipe handed to [`SecureListener::accept_pipe`] ends up.
#[derive(uniffi::Object)]
pub struct PipeAdmission(Mutex<Option<PipeHandle>>);

#[uniffi::export]
impl PipeAdmission {
    /// Resolves when the pipe was accepted (its channel then arrives through
    /// `accept`) or given up. Asked a second time, it reports `Refused`.
    pub async fn outcome(&self) -> PipeOutcome {
        let handle = self.0.lock().ok().and_then(|mut handle| handle.take());
        match handle {
            Some(handle) => handle.outcome().await,
            None => PipeOutcome::Refused,
        }
    }
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

impl SecureListener {
    /// Hands over a pipe a USB provider opened to a device. The listener is the
    /// TLS server on it, with the same identity, pins, admission bounds, trust
    /// fence and lane rule as a TCP connection; the channel arrives through
    /// [`Self::accept`]. The pipe may be handed over before the device dials;
    /// the handle reports how it ended up (see
    /// [`SessionListener::accept_stream`]).
    pub fn accept_stream(&self, pipe: UsbPipe) -> Result<PipeHandle, ChannelError> {
        let UsbPipe { io, medium, device } = pipe;
        let listener = self
            .state
            .lock()
            .map_err(|_| ChannelError::Closed)?
            .listener
            .clone()
            .ok_or(ChannelError::Closed)?;
        listener
            .accept_stream(io, PeerLocator::Usb { medium, device })
            .map_err(|error| match error {
                TransportError::Closed => ChannelError::Closed,
                TransportError::Busy => ChannelError::Busy,
                _ => ChannelError::ConnectionFailed,
            })
    }
}

#[uniffi::export]
impl SecureListener {
    /// [`Self::accept_stream`] for a native host: hands over a pipe and
    /// returns its outcome's handle. A pipe handle is used once; a second use
    /// is `InvalidConfiguration`.
    pub fn accept_pipe(
        &self,
        pipe: Arc<crate::duplex::UsbPipeHandle>,
    ) -> Result<Arc<PipeAdmission>, ChannelError> {
        let pipe = pipe.take().ok_or(ChannelError::InvalidConfiguration)?;
        let handle = self.accept_stream(pipe)?;
        Ok(Arc::new(PipeAdmission(Mutex::new(Some(handle)))))
    }

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
/// messages pick a lane listen with [`listen_secure_with`]. Exported to
/// UniFFI, like [`connect_secure`], for a native host that needs no lane
/// policy of its own.
#[uniffi::export]
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

/// [`connect_secure_stream_with`] with StreamCore's binary lanes only, for a
/// native client: connects over a pipe this device accepted from the host.
/// A handle is used once; a second use is `InvalidConfiguration`.
#[uniffi::export]
pub async fn connect_secure_pipe(
    identity: Arc<PeerIdentity>,
    trusted_peer: Vec<u8>,
    pipe: Arc<crate::duplex::UsbPipeHandle>,
    options: SessionOptions,
    cancellation: Arc<crate::OperationCancellation>,
) -> Result<Arc<SecureChannel>, ChannelError> {
    let pipe = pipe.take().ok_or(ChannelError::InvalidConfiguration)?;
    connect_secure_stream_with(
        binary_lanes,
        identity,
        trusted_peer,
        pipe,
        options,
        cancellation,
    )
    .await
}

/// Connects with StreamCore's binary lanes only. Applications whose control
/// messages pick a lane connect with [`connect_secure_with`]. Exported to
/// UniFFI so a native client that needs no lane policy of its own (the
/// Android video harness) can connect without a Rust crate of its own.
#[uniffi::export]
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
                    // A pipe is connected with `connect_secure_stream_with`.
                    ChannelTransport::TlsStream { .. } => {
                        return Err(ChannelError::InvalidConfiguration);
                    }
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

/// Connects over a pipe the device side accepted from a USB provider: the
/// host is always the TLS server, with the same pins as on the network. One
/// attempt and no retry; the channel routes application frames with `lanes`.
pub async fn connect_secure_stream_with(
    lanes: LanePolicy,
    identity: Arc<PeerIdentity>,
    trusted_peer: Vec<u8>,
    pipe: UsbPipe,
    options: SessionOptions,
    cancellation: Arc<crate::OperationCancellation>,
) -> Result<Arc<SecureChannel>, ChannelError> {
    let UsbPipe { io, medium, device } = pipe;
    on_runtime(async move {
        cancellation
            .run(transport::dial_pipe(
                &identity,
                trusted_peer,
                io,
                PeerLocator::Usb { medium, device },
                options.hello(),
            ))
            .await
            .ok_or(ChannelError::Closed)?
            .map(|session| SecureChannel::from_session(session, lanes))
            .map_err(|_| ChannelError::ConnectionFailed)
    })
    .await
}
