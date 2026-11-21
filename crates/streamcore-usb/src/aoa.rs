//! The accessory link layer (STREAMCORE-VIDEO 8.4, AOA): an Android accessory
//! connection is one pair of bulk endpoints with no notion of a connection, so
//! this layer frames it into pipes. The host opens a pipe per service; each is
//! a byte stream the secure transport runs its pinned TLS on, like any other
//! pipe. The platform adapter (IOUSBHost on macOS, the accessory file on
//! Android, a USB library on Linux) only supplies the raw byte stream.
//!
//! Every frame has a 16-byte header: magic (2), kind (1), flags (1), sequence
//! (4), session (4), payload length (4), big-endian. The first frame each
//! side sends is `linkHello` with the link version and its role. A session's
//! `open` or `opened` has sequence 0 and its data frames count up from 1 in
//! each direction: a gap or a duplicate closes that session. A bad header
//! means the framing is lost, which ends the link and every pipe on it.
//!
//! The link is not a security boundary. Whatever is on the other end of the
//! cable can speak this framing; the handshake on each pipe decides trust.
//!
//! One slow pipe holds back the others: frames are delivered in order, and a
//! pipe nobody reads fills its queue and stalls the link until it is dropped
//! (the transport's own liveness drops a dead channel within seconds).

use crate::{UsbError, UsbService, device::MAX_DEVICE_PIPES};
use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use streamcore_transport::transport::PipeIo;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    sync::{mpsc, oneshot, watch},
};

/// The link version both sides must share.
pub const LINK_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 16;
/// A frame with its header fits one 16 KiB accessory read.
pub const MAX_PAYLOAD: usize = 16 * 1024 - HEADER_LEN;

const MAGIC: [u8; 2] = [0x53, 0x4C];
/// Frames queued for one pipe before the link waits for its reader (about
/// 1 MiB).
const PIPE_QUEUE: usize = 64;
/// Frames queued for the cable.
const WRITE_QUEUE: usize = 64;
/// How long the host waits for the device to answer an `open`.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Kind {
    LinkHello = 1,
    Open = 2,
    Opened = 3,
    Data = 4,
    Close = 5,
    Ping = 6,
    Pong = 7,
    Pad = 8,
}

impl Kind {
    fn from_wire(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::LinkHello,
            2 => Self::Open,
            3 => Self::Opened,
            4 => Self::Data,
            5 => Self::Close,
            6 => Self::Ping,
            7 => Self::Pong,
            8 => Self::Pad,
            _ => return None,
        })
    }
}

/// Which end of the cable this is. The host opens pipes, the device accepts
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ffi", derive(uniffi::Enum))]
pub enum LinkRole {
    Host,
    Device,
}

impl LinkRole {
    fn wire(self) -> u8 {
        match self {
            Self::Host => 1,
            Self::Device => 2,
        }
    }
    fn peer(self) -> Self {
        match self {
            Self::Host => Self::Device,
            Self::Device => Self::Host,
        }
    }
}

struct Frame {
    kind: Kind,
    sequence: u32,
    session: u32,
    payload: Vec<u8>,
}

impl Frame {
    fn control(kind: Kind, session: u32, payload: Vec<u8>) -> Self {
        Self {
            kind,
            sequence: 0,
            session,
            payload,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend(MAGIC);
        out.push(self.kind as u8);
        out.push(0);
        out.extend(self.sequence.to_be_bytes());
        out.extend(self.session.to_be_bytes());
        out.extend((self.payload.len() as u32).to_be_bytes());
        out.extend(&self.payload);
        out
    }
}

fn service_wire(service: UsbService) -> u8 {
    match service {
        UsbService::Session => 1,
        UsbService::Pairing => 2,
    }
}

fn service_from_wire(byte: u8) -> Option<UsbService> {
    match byte {
        1 => Some(UsbService::Session),
        2 => Some(UsbService::Pairing),
        _ => None,
    }
}

/// Reads one frame. `Ok(None)` is a clean end of the stream between frames.
async fn read_frame(io: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    match io.read(&mut header[..1]).await? {
        0 => return Ok(None),
        _ => io.read_exact(&mut header[1..]).await?,
    };
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    if header[..2] != MAGIC || header[3] != 0 {
        return Err(invalid());
    }
    let kind = Kind::from_wire(header[2]).ok_or_else(invalid)?;
    let word = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let length = word(12) as usize;
    if length > MAX_PAYLOAD {
        return Err(invalid());
    }
    let mut payload = vec![0u8; length];
    io.read_exact(&mut payload).await?;
    Ok(Some(Frame {
        kind,
        sequence: word(4),
        session: word(8),
        payload,
    }))
}

struct Session {
    /// Data for the pipe's reader; dropped to signal the end of the stream.
    data: mpsc::Sender<Vec<u8>>,
    /// The next data sequence expected from the peer.
    next_in: u32,
    /// Host: resolves when the device answers the `open`.
    opened: Option<oneshot::Sender<bool>>,
    /// Device, or host after `opened`: data may flow.
    open: bool,
}

struct Shared {
    role: LinkRole,
    sessions: Mutex<HashMap<u32, Session>>,
    out: mpsc::Sender<Frame>,
    next_session: AtomicU32,
    closed: watch::Sender<bool>,
    dead: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    /// The link is over: every pipe sees the end of its stream.
    fn end(&self) {
        self.dead.store(true, Ordering::SeqCst);
        lock(&self.sessions).clear();
        self.closed.send_replace(true);
    }

    /// Forget `session` and tell the peer, if it was known.
    fn close_session(&self, session: u32) {
        if lock(&self.sessions).remove(&session).is_some() {
            // A full queue means the cable is stalled; the peer then learns of
            // the close when the link ends.
            let _ = self
                .out
                .try_send(Frame::control(Kind::Close, session, Vec::new()));
        }
    }
}

/// A pipe a device accepted, and the service the host asked for.
pub struct AcceptedPipe {
    pub service: UsbService,
    pub io: PipeIo,
}

/// One accessory connection, framed into pipes. Dropping it ends the link.
pub struct AccessoryLink {
    shared: Arc<Shared>,
    accepted: tokio::sync::Mutex<mpsc::Receiver<AcceptedPipe>>,
}

/// A task of the link, for whoever runs it.
pub type LinkTask = Pin<Box<dyn Future<Output = ()> + Send>>;

impl AccessoryLink {
    /// Starts the link on `io`, the accessory's raw byte stream, on the
    /// current runtime. The link hello is exchanged in the background: a peer
    /// with another version or the same role ends the link.
    pub fn start(io: PipeIo, role: LinkRole) -> Self {
        Self::start_with(io, role, |task| {
            tokio::spawn(task);
        })
    }

    /// [`Self::start`] with the link's two tasks handed to `spawn`. They end
    /// when the link does.
    pub fn start_with(io: PipeIo, role: LinkRole, spawn: impl Fn(LinkTask)) -> Self {
        let (mut reader, mut writer) = tokio::io::split(io);
        let (out, mut frames) = mpsc::channel::<Frame>(WRITE_QUEUE);
        let (accept_tx, accept_rx) = mpsc::channel(MAX_DEVICE_PIPES);
        let shared = Arc::new(Shared {
            role,
            sessions: Mutex::new(HashMap::new()),
            out,
            next_session: AtomicU32::new(1),
            closed: watch::Sender::new(false),
            dead: AtomicBool::new(false),
        });
        let mut hello = LINK_VERSION.to_be_bytes().to_vec();
        hello.push(role.wire());
        // The queue is empty: this cannot fail.
        let _ = shared
            .out
            .try_send(Frame::control(Kind::LinkHello, 0, hello));

        let write_shared = shared.clone();
        spawn(Box::pin(async move {
            let mut closed = write_shared.closed.subscribe();
            loop {
                let frame = tokio::select! {
                    frame = frames.recv() => frame,
                    _ = closed.wait_for(|closed| *closed) => None,
                };
                let Some(frame) = frame else { break };
                // One write per frame: header and payload leave together.
                if writer.write_all(&frame.encode()).await.is_err() || writer.flush().await.is_err()
                {
                    break;
                }
            }
            let _ = writer.shutdown().await;
            write_shared.end();
        }));
        let read_shared = shared.clone();
        spawn(Box::pin(async move {
            let mut closed = read_shared.closed.subscribe();
            tokio::select! {
                () = read_loop(&mut reader, &read_shared, &accept_tx) => {}
                _ = closed.wait_for(|closed| *closed) => {}
            }
            read_shared.end();
        }));
        Self {
            shared,
            accepted: tokio::sync::Mutex::new(accept_rx),
        }
    }

    /// Host: opens a pipe to `service` on the device.
    pub async fn open(&self, service: UsbService) -> Result<PipeIo, UsbError> {
        let shared = &self.shared;
        if shared.role != LinkRole::Host {
            return Err(UsbError::Protocol);
        }
        let id = shared.next_session.fetch_add(1, Ordering::SeqCst);
        if id == 0 || id == u32::MAX {
            // The id space of this link is used up: only a new link helps.
            shared.end();
            return Err(UsbError::Protocol);
        }
        let (data, queue) = mpsc::channel(PIPE_QUEUE);
        let (opened, answer) = oneshot::channel();
        {
            let mut sessions = lock(&shared.sessions);
            if shared.dead.load(Ordering::SeqCst) {
                return Err(link_gone());
            }
            sessions.insert(
                id,
                Session {
                    data,
                    next_in: 1,
                    opened: Some(opened),
                    open: false,
                },
            );
        }
        let pipe = LinkPipe::new(shared.clone(), id, queue);
        let open = Frame::control(Kind::Open, id, vec![service_wire(service)]);
        if shared.out.send(open).await.is_err() {
            return Err(link_gone());
        }
        match tokio::time::timeout(OPEN_TIMEOUT, answer).await {
            Ok(Ok(true)) => Ok(Box::new(pipe)),
            Ok(Ok(false)) => Err(UsbError::Refused("the device refused the pipe".into())),
            Ok(Err(_)) => Err(link_gone()),
            Err(_) => Err(UsbError::Timeout),
        }
    }

    /// Device: the next pipe the host opened, or `None` when the link ended.
    pub async fn accept(&self) -> Option<AcceptedPipe> {
        self.accepted.lock().await.recv().await
    }

    /// Resolves when the link has ended (the cable, a framing error, or
    /// [`Self::close`]).
    pub async fn closed(&self) {
        let mut closed = self.shared.closed.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    pub fn is_closed(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Ends the link and every pipe on it.
    pub fn close(&self) {
        self.shared.end();
    }
}

impl Drop for AccessoryLink {
    fn drop(&mut self) {
        self.shared.end();
    }
}

fn link_gone() -> UsbError {
    UsbError::Io(io::ErrorKind::BrokenPipe.into())
}

async fn read_loop(
    reader: &mut (impl AsyncRead + Unpin),
    shared: &Arc<Shared>,
    accept: &mpsc::Sender<AcceptedPipe>,
) {
    // The first frame is the peer's hello.
    match read_frame(reader).await {
        Ok(Some(frame))
            if frame.kind == Kind::LinkHello
                && frame.session == 0
                && frame.payload.len() == 3
                && frame.payload[..2] == LINK_VERSION.to_be_bytes()
                && frame.payload[2] == shared.role.peer().wire() => {}
        _ => return,
    }
    while let Ok(Some(frame)) = read_frame(reader).await {
        let id = frame.session;
        match frame.kind {
            // A second hello means the peer restarted under us.
            Kind::LinkHello => return,
            Kind::Pad => {}
            Kind::Ping => {
                if shared
                    .out
                    .send(Frame::control(Kind::Pong, 0, frame.payload))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Kind::Pong => {}
            Kind::Open => {
                let service = match frame.payload.as_slice() {
                    [byte] => service_from_wire(*byte),
                    _ => None,
                };
                if shared.role != LinkRole::Device || id == 0 || frame.sequence != 0 {
                    return;
                }
                let refuse = Frame::control(Kind::Close, id, Vec::new());
                let Some(service) = service else {
                    // A service this version does not know.
                    if shared.out.send(refuse).await.is_err() {
                        return;
                    }
                    continue;
                };
                let (data, queue) = mpsc::channel(PIPE_QUEUE);
                let room = {
                    let mut sessions = lock(&shared.sessions);
                    if sessions.contains_key(&id) {
                        // An id is used once per link.
                        return;
                    }
                    let room = sessions.len() < MAX_DEVICE_PIPES;
                    if room {
                        sessions.insert(
                            id,
                            Session {
                                data,
                                next_in: 1,
                                opened: None,
                                open: true,
                            },
                        );
                    }
                    room
                };
                if !room {
                    if shared.out.send(refuse).await.is_err() {
                        return;
                    }
                    continue;
                }
                let pipe = LinkPipe::new(shared.clone(), id, queue);
                if shared
                    .out
                    .send(Frame::control(Kind::Opened, id, Vec::new()))
                    .await
                    .is_err()
                {
                    return;
                }
                let accepted = AcceptedPipe {
                    service,
                    io: Box::new(pipe),
                };
                // Nobody accepting: the pipe is dropped, which closes it.
                let _ = accept.try_send(accepted);
            }
            Kind::Opened => {
                if shared.role != LinkRole::Host || frame.sequence != 0 {
                    return;
                }
                let waiter = lock(&shared.sessions).get_mut(&id).and_then(|session| {
                    session.open = true;
                    session.opened.take()
                });
                if let Some(waiter) = waiter {
                    let _ = waiter.send(true);
                }
            }
            Kind::Close => {
                let session = lock(&shared.sessions).remove(&id);
                if let Some(waiter) = session.and_then(|session| session.opened) {
                    let _ = waiter.send(false);
                }
            }
            Kind::Data => {
                let queue = {
                    let mut sessions = lock(&shared.sessions);
                    match sessions.get_mut(&id) {
                        // Late data for a pipe closed here: ignored.
                        None => continue,
                        Some(session) if session.open && frame.sequence == session.next_in => {
                            session.next_in = session.next_in.wrapping_add(1);
                            if session.next_in == 0 {
                                // The sequence space is used up: end the pipe.
                                None
                            } else {
                                Some(session.data.clone())
                            }
                        }
                        // A gap, a duplicate, or data before `opened`.
                        Some(_) => None,
                    }
                };
                let Some(queue) = queue else {
                    shared.close_session(id);
                    continue;
                };
                if frame.payload.is_empty() {
                    continue;
                }
                // Waits for the pipe's reader: the stream's backpressure.
                if queue.send(frame.payload).await.is_err() {
                    shared.close_session(id);
                }
            }
        }
    }
}

type Sending = Pin<Box<dyn Future<Output = bool> + Send>>;

/// One pipe of a link, as a byte stream.
struct LinkPipe {
    shared: Arc<Shared>,
    id: u32,
    queue: mpsc::Receiver<Vec<u8>>,
    /// The rest of a frame the reader's buffer had no room for.
    rest: Vec<u8>,
    next_out: u32,
    /// A frame on its way to the cable's queue, and how many bytes of the
    /// caller's buffer it holds.
    sending: Option<(Sending, usize)>,
}

impl LinkPipe {
    fn new(shared: Arc<Shared>, id: u32, queue: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            shared,
            id,
            queue,
            rest: Vec::new(),
            next_out: 1,
            sending: None,
        }
    }

    fn live(&self) -> bool {
        lock(&self.shared.sessions).contains_key(&self.id)
    }
}

impl AsyncRead for LinkPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.rest.is_empty() {
            match self.queue.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                // The session ended: the end of the stream.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(data)) => self.rest = data,
            }
        }
        let take = self.rest.len().min(buf.remaining());
        buf.put_slice(&self.rest[..take]);
        self.rest.drain(..take);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for LinkPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.sending.is_none() {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            if !self.live() {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            let take = buf.len().min(MAX_PAYLOAD);
            let frame = Frame {
                kind: Kind::Data,
                sequence: self.next_out,
                session: self.id,
                payload: buf[..take].to_vec(),
            };
            self.next_out = self.next_out.wrapping_add(1);
            let out = self.shared.out.clone();
            self.sending = Some((Box::pin(async move { out.send(frame).await.is_ok() }), take));
        }
        let Some((sending, taken)) = self.sending.as_mut() else {
            return Poll::Pending;
        };
        let taken = *taken;
        match sending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(sent) => {
                self.sending = None;
                if !sent || self.next_out == 0 {
                    self.shared.close_session(self.id);
                    return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                }
                Poll::Ready(Ok(taken))
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // A frame handed to the cable's queue is written by the link's task.
        let Some((sending, _)) = self.sending.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match sending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            // `poll_write` reports the frame's bytes on its next call.
            Poll::Ready(sent) => {
                let done: Sending = Box::pin(std::future::ready(sent));
                if let Some((sending, _)) = self.sending.as_mut() {
                    *sending = done;
                }
                Poll::Ready(Ok(()))
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shared.close_session(self.id);
        Poll::Ready(Ok(()))
    }
}

impl Drop for LinkPipe {
    fn drop(&mut self) {
        self.shared.close_session(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn links() -> (AccessoryLink, AccessoryLink) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (
            AccessoryLink::start(Box::new(a), LinkRole::Host),
            AccessoryLink::start(Box::new(b), LinkRole::Device),
        )
    }

    /// A host link and the raw other end of its cable.
    fn host_and_wire() -> (AccessoryLink, DuplexStream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (AccessoryLink::start(Box::new(a), LinkRole::Host), b)
    }

    fn hello(role: LinkRole) -> Frame {
        let mut payload = LINK_VERSION.to_be_bytes().to_vec();
        payload.push(role.wire());
        Frame::control(Kind::LinkHello, 0, payload)
    }

    async fn expect(wire: &mut DuplexStream, kind: Kind) -> Frame {
        let frame = read_frame(wire).await.unwrap().expect("a frame");
        assert_eq!(frame.kind, kind);
        frame
    }

    #[tokio::test]
    async fn a_pipe_carries_bytes_both_ways_and_ends_cleanly() {
        let (host, device) = links();
        let (mut near, accepted) = tokio::join!(
            async { host.open(UsbService::Session).await.unwrap() },
            device.accept()
        );
        let accepted = accepted.unwrap();
        assert_eq!(accepted.service, UsbService::Session);
        let mut far = accepted.io;

        near.write_all(b"from the host").await.unwrap();
        let mut got = [0u8; 13];
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"from the host");
        far.write_all(b"from the device").await.unwrap();
        let mut got = [0u8; 15];
        near.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"from the device");

        // Dropping one end is the end of the stream at the other.
        drop(near);
        let mut rest = Vec::new();
        assert_eq!(far.read_to_end(&mut rest).await.unwrap(), 0);
        assert!(!host.is_closed() && !device.is_closed());
    }

    #[tokio::test]
    async fn large_writes_arrive_whole_and_in_order_on_two_pipes_at_once() {
        let (host, device) = links();
        let mut pipes = Vec::new();
        for service in [UsbService::Session, UsbService::Pairing] {
            let (near, far) =
                tokio::join!(async { host.open(service).await.unwrap() }, device.accept());
            let far = far.unwrap();
            assert_eq!(far.service, service);
            pipes.push((near, far.io));
        }
        let mut work = Vec::new();
        for (index, (mut near, mut far)) in pipes.into_iter().enumerate() {
            let data: Vec<u8> = (0..300_000u32).map(|i| (i as u8) ^ (index as u8)).collect();
            let expected = data.clone();
            work.push(tokio::spawn(async move {
                near.write_all(&data).await.unwrap();
                near.shutdown().await.unwrap();
            }));
            work.push(tokio::spawn(async move {
                let mut got = Vec::new();
                far.read_to_end(&mut got).await.unwrap();
                assert!(got == expected, "pipe {index} differs");
            }));
        }
        for task in work {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_device_refuses_more_pipes_than_it_holds() {
        let (host, device) = links();
        let mut held = Vec::new();
        for _ in 0..MAX_DEVICE_PIPES {
            let near = host.open(UsbService::Session).await.unwrap();
            held.push((near, device.accept().await.unwrap()));
        }
        assert!(matches!(
            host.open(UsbService::Session).await,
            Err(UsbError::Refused(_))
        ));
        // A closed pipe makes room again.
        held.pop();
        tokio::task::yield_now().await;
        let mut again = None;
        for _ in 0..50 {
            match host.open(UsbService::Session).await {
                Ok(pipe) => {
                    again = Some(pipe);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        assert!(again.is_some());
    }

    #[tokio::test]
    async fn only_the_host_opens_pipes() {
        let (_host, device) = links();
        assert!(matches!(
            device.open(UsbService::Session).await,
            Err(UsbError::Protocol)
        ));
    }

    #[tokio::test]
    async fn another_link_version_or_the_same_role_ends_the_link() {
        for payload in [vec![0, 2, 2], vec![0, 1, 1], vec![0, 1]] {
            let (host, mut wire) = host_and_wire();
            expect(&mut wire, Kind::LinkHello).await;
            let bad = Frame::control(Kind::LinkHello, 0, payload);
            wire.write_all(&bad.encode()).await.unwrap();
            host.closed().await;
            assert!(matches!(
                host.open(UsbService::Session).await,
                Err(UsbError::Io(_))
            ));
        }
    }

    #[tokio::test]
    async fn a_bad_header_ends_the_link_and_every_pipe() {
        let (host, mut wire) = host_and_wire();
        expect(&mut wire, Kind::LinkHello).await;
        wire.write_all(&hello(LinkRole::Device).encode())
            .await
            .unwrap();
        let (pipe, ()) = tokio::join!(host.open(UsbService::Session), async {
            let open = expect(&mut wire, Kind::Open).await;
            let opened = Frame::control(Kind::Opened, open.session, Vec::new());
            wire.write_all(&opened.encode()).await.unwrap();
        });
        let mut pipe = pipe.unwrap();
        wire.write_all(&[0xFF; HEADER_LEN]).await.unwrap();
        host.closed().await;
        let mut rest = Vec::new();
        assert_eq!(pipe.read_to_end(&mut rest).await.unwrap(), 0);
        assert!(pipe.write_all(b"x").await.is_err());
    }

    #[tokio::test]
    async fn a_gap_or_a_duplicate_closes_that_pipe_only() {
        for second in [3u32, 1] {
            let (host, mut wire) = host_and_wire();
            expect(&mut wire, Kind::LinkHello).await;
            wire.write_all(&hello(LinkRole::Device).encode())
                .await
                .unwrap();
            let mut pipes = Vec::new();
            let mut ids = Vec::new();
            for _ in 0..2 {
                let (pipe, id) = tokio::join!(host.open(UsbService::Session), async {
                    let open = expect(&mut wire, Kind::Open).await;
                    let opened = Frame::control(Kind::Opened, open.session, Vec::new());
                    wire.write_all(&opened.encode()).await.unwrap();
                    open.session
                });
                pipes.push(pipe.unwrap());
                ids.push(id);
            }
            let data = |session, sequence, byte: u8| Frame {
                kind: Kind::Data,
                sequence,
                session,
                payload: vec![byte],
            };
            wire.write_all(&data(ids[0], 1, b'a').encode())
                .await
                .unwrap();
            wire.write_all(&data(ids[0], second, b'b').encode())
                .await
                .unwrap();
            wire.write_all(&data(ids[1], 1, b'c').encode())
                .await
                .unwrap();

            let mut got = Vec::new();
            pipes[0].read_to_end(&mut got).await.unwrap();
            assert_eq!(got, b"a", "the broken pipe ends after its good frame");
            let close = expect(&mut wire, Kind::Close).await;
            assert_eq!(close.session, ids[0]);
            let mut one = [0u8; 1];
            pipes[1].read_exact(&mut one).await.unwrap();
            assert_eq!(&one, b"c");
            assert!(!host.is_closed());
        }
    }

    #[tokio::test]
    async fn a_device_that_does_not_answer_an_open_times_out() {
        tokio::time::pause();
        let (host, mut wire) = host_and_wire();
        expect(&mut wire, Kind::LinkHello).await;
        wire.write_all(&hello(LinkRole::Device).encode())
            .await
            .unwrap();
        assert!(matches!(
            host.open(UsbService::Session).await,
            Err(UsbError::Timeout)
        ));
    }

    #[tokio::test]
    async fn a_ping_is_answered_and_padding_is_ignored() {
        let (_host, mut wire) = host_and_wire();
        expect(&mut wire, Kind::LinkHello).await;
        wire.write_all(&hello(LinkRole::Device).encode())
            .await
            .unwrap();
        wire.write_all(&Frame::control(Kind::Pad, 0, vec![0; 100]).encode())
            .await
            .unwrap();
        wire.write_all(&Frame::control(Kind::Ping, 0, vec![7, 8]).encode())
            .await
            .unwrap();
        assert_eq!(expect(&mut wire, Kind::Pong).await.payload, vec![7, 8]);
    }

    #[tokio::test]
    async fn the_cable_going_away_ends_the_link() {
        let (host, wire) = host_and_wire();
        drop(wire);
        host.closed().await;
        assert!(host.is_closed());
    }
}
