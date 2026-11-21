//! A byte stream the platform supplies (STREAMCORE-VIDEO 8.2): the raw bulk
//! endpoints of an accessory connection exist only in native code, so the
//! consumer wraps them in a [`ByteDuplex`] and Rust turns that into a pipe.
//!
//! The calls block. Each duplex gets one reading and one writing thread, so a
//! blocked platform read never holds a runtime worker, and dropping the pipe
//! calls [`ByteDuplex::shutdown`], which must make a blocked call return.

use crate::transport::{Medium, PipeIo};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc, oneshot},
};

/// The most bytes asked of one `read` and passed to one `write`.
pub const DUPLEX_CHUNK: usize = 64 * 1024;
/// Chunks waiting for the platform's `write`: small, so a slow cable pushes
/// back on the sender instead of filling a queue.
const WRITE_QUEUE: usize = 2;
/// How long a graceful shutdown waits for queued writes to reach the platform.
const DRAIN_LIMIT: Duration = Duration::from_secs(1);
/// Chunks read and not yet consumed.
const READ_QUEUE: usize = 4;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum DuplexError {
    /// The stream ended or was closed: the end of the pipe, not a failure.
    #[error("the stream is closed")]
    Closed,
    #[error("the stream failed")]
    Failed,
}

impl From<uniffi::UnexpectedUniFFICallbackError> for DuplexError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed
    }
}

/// A raw byte stream the platform owns. The calls block, on threads Rust
/// owns (`streamcore-usb-rd` and `streamcore-usb-wr`, one pair per stream).
///
/// What an implementation must honour:
/// - `read` is never called while another `read` runs, nor `write` while
///   another `write` runs, but a `read` and a `write` do run at the same
///   time.
/// - `shutdown` comes from any thread, while a `read` or `write` is blocked or
///   not, more than once, and after the end of the stream. It must be safe
///   every time and make a blocked call return within a bounded time (abort
///   the endpoint's I/O, close the descriptor): a read that stays blocked
///   holds its thread for the life of the process.
#[uniffi::export(rust, foreign)]
pub trait ByteDuplex: Send + Sync {
    /// Blocks until at least one byte arrived and returns at most `max`
    /// bytes. An empty result or [`DuplexError::Closed`] is the end of the
    /// stream.
    fn read(&self, max: u32) -> Result<Vec<u8>, DuplexError>;
    /// Blocks until the platform has taken all of `data`.
    fn write(&self, data: Vec<u8>) -> Result<(), DuplexError>;
    /// Ends the stream: a blocked `read` or `write` returns (as closed), and
    /// later calls fail. (Not named `close`: generated Kotlin and Swift
    /// objects already have one.)
    fn shutdown(&self);
}

/// A pipe ready for the secure transport, with what carries it and the
/// provider's label for the device (local display only, never identity). It
/// is used once: by [`crate::channel::SecureListener::accept_pipe`] on a host
/// or [`crate::channel::connect_secure_pipe`] on a device.
#[derive(uniffi::Object)]
pub struct UsbPipeHandle {
    pipe: Mutex<Option<crate::channel::UsbPipe>>,
}

impl UsbPipeHandle {
    pub fn new(pipe: crate::channel::UsbPipe) -> Arc<Self> {
        Arc::new(Self {
            pipe: Mutex::new(Some(pipe)),
        })
    }

    /// The pipe, the first time it is asked for.
    pub fn take(&self) -> Option<crate::channel::UsbPipe> {
        self.pipe.lock().ok()?.take()
    }
}

#[uniffi::export]
impl UsbPipeHandle {
    /// A pipe that is the whole of `io`: a stream that needs no framing of
    /// its own. An accessory connection carries several pipes and goes
    /// through its link layer instead.
    #[uniffi::constructor]
    pub fn from_duplex(io: Arc<dyn ByteDuplex>, medium: Medium, device: String) -> Arc<Self> {
        Self::new(crate::channel::UsbPipe {
            io: pipe_from_duplex(io),
            medium,
            device,
        })
    }
}

/// Wraps a platform stream as a pipe. False from the thread spawn leaves a
/// pipe that reads as ended.
pub fn pipe_from_duplex(io: Arc<dyn ByteDuplex>) -> PipeIo {
    let (read_tx, read_rx) = mpsc::channel::<io::Result<Vec<u8>>>(READ_QUEUE);
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
    let reader = io.clone();
    let spawned = std::thread::Builder::new()
        .name("streamcore-usb-rd".into())
        .spawn(move || {
            loop {
                match reader.read(DUPLEX_CHUNK as u32) {
                    Ok(data) if data.is_empty() => break,
                    Ok(data) if data.len() <= DUPLEX_CHUNK => {
                        if read_tx.blocking_send(Ok(data)).is_err() {
                            break;
                        }
                    }
                    Err(DuplexError::Closed) => break,
                    // More than asked for breaks the contract: cutting it
                    // short would corrupt the stream, so the pipe fails.
                    Ok(_) | Err(DuplexError::Failed) => {
                        let _ = read_tx.blocking_send(Err(io::ErrorKind::Other.into()));
                        break;
                    }
                }
            }
            reader.shutdown();
        })
        .is_ok();
    let writer = io.clone();
    let (drained_tx, drained) = oneshot::channel::<()>();
    let spawned = spawned
        && std::thread::Builder::new()
            .name("streamcore-usb-wr".into())
            .spawn(move || {
                // Dropped when this thread ends: everything queued was
                // written, or the stream failed.
                let _drained = drained_tx;
                while let Some(mut data) = write_rx.blocking_recv() {
                    // Fewer, larger calls: each one copies across the binding.
                    while data.len() < DUPLEX_CHUNK {
                        match write_rx.try_recv() {
                            Ok(more) if data.len() + more.len() <= DUPLEX_CHUNK => {
                                data.extend(more);
                            }
                            Ok(more) => {
                                if writer.write(std::mem::replace(&mut data, more)).is_err() {
                                    writer.shutdown();
                                    return;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if writer.write(data).is_err() {
                        break;
                    }
                }
                writer.shutdown();
            })
            .is_ok();
    if !spawned {
        io.shutdown();
    }
    Box::new(DuplexPipe {
        io,
        read: read_rx,
        rest: Vec::new(),
        write: Some(write_tx),
        sending: None,
        drained,
        drain_limit: None,
    })
}

type Sending = Pin<Box<dyn Future<Output = bool> + Send>>;

struct DuplexPipe {
    io: Arc<dyn ByteDuplex>,
    read: mpsc::Receiver<io::Result<Vec<u8>>>,
    rest: Vec<u8>,
    /// Gone once the pipe is shut down.
    write: Option<mpsc::Sender<Vec<u8>>>,
    /// A chunk on its way to the write queue, and its length.
    sending: Option<(Sending, usize)>,
    /// Resolves when the writing thread has ended.
    drained: oneshot::Receiver<()>,
    drain_limit: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl AsyncRead for DuplexPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.rest.is_empty() {
            match self.read.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(Some(Ok(data))) => self.rest = data,
            }
        }
        let take = self.rest.len().min(buf.remaining());
        buf.put_slice(&self.rest[..take]);
        self.rest.drain(..take);
        Poll::Ready(Ok(()))
    }
}

impl DuplexPipe {
    fn poll_sending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let Some((sending, taken)) = self.sending.as_mut() else {
            return Poll::Ready(Ok(0));
        };
        let taken = *taken;
        match sending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(sent) => {
                self.sending = None;
                if sent {
                    Poll::Ready(Ok(taken))
                } else {
                    Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
                }
            }
        }
    }
}

impl AsyncWrite for DuplexPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.sending.is_none() {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let Some(queue) = self.write.clone() else {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            };
            let take = buf.len().min(DUPLEX_CHUNK);
            let chunk = buf[..take].to_vec();
            self.sending = Some((
                Box::pin(async move { queue.send(chunk).await.is_ok() }),
                take,
            ));
        }
        self.poll_sending(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        // A chunk in the queue is written by the writing thread; a chunk not
        // yet queued is reported by the `poll_write` that owns it.
        Poll::Ready(Ok(()))
    }

    /// A graceful end: what was written before it (a TLS close, a last
    /// message) reaches the platform first, within [`DRAIN_LIMIT`]. Dropping
    /// the pipe instead ends the stream at once.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.sending.is_some() && self.poll_sending(cx).is_pending() {
            return Poll::Pending;
        }
        // The writing thread ends once its queue is written.
        self.write = None;
        let this = &mut *self;
        let limit = this
            .drain_limit
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(DRAIN_LIMIT)));
        if Pin::new(&mut this.drained).poll(cx).is_pending() && limit.as_mut().poll(cx).is_pending()
        {
            return Poll::Pending;
        }
        this.io.shutdown();
        Poll::Ready(Ok(()))
    }
}

impl Drop for DuplexPipe {
    fn drop(&mut self) {
        // Unblocks the platform's pending read.
        self.io.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Condvar,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A platform stream as a fake: reads come from `incoming`, writes are
    /// recorded, `shutdown` wakes a blocked read.
    #[derive(Default)]
    struct Fake {
        incoming: Mutex<Vec<Result<Vec<u8>, bool>>>,
        wake: Condvar,
        closed: AtomicBool,
        closes: AtomicUsize,
        written: Mutex<Vec<Vec<u8>>>,
        fail_writes: AtomicBool,
        write_gate: Mutex<bool>,
        write_wake: Condvar,
    }

    impl Fake {
        fn feed(&self, data: &[u8]) {
            self.incoming.lock().unwrap().push(Ok(data.to_vec()));
            self.wake.notify_all();
        }
        fn fail_read(&self) {
            self.incoming.lock().unwrap().push(Err(false));
            self.wake.notify_all();
        }
        fn written(&self) -> Vec<u8> {
            self.written.lock().unwrap().concat()
        }
    }

    impl ByteDuplex for Fake {
        fn read(&self, max: u32) -> Result<Vec<u8>, DuplexError> {
            let mut incoming = self.incoming.lock().unwrap();
            loop {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(DuplexError::Closed);
                }
                if !incoming.is_empty() {
                    return match incoming.remove(0) {
                        Ok(data) => {
                            assert!(max as usize == DUPLEX_CHUNK);
                            Ok(data)
                        }
                        Err(_) => Err(DuplexError::Failed),
                    };
                }
                incoming = self.wake.wait(incoming).unwrap();
            }
        }
        fn write(&self, data: Vec<u8>) -> Result<(), DuplexError> {
            let mut held = self.write_gate.lock().unwrap();
            while *held && !self.closed.load(Ordering::SeqCst) {
                held = self.write_wake.wait(held).unwrap();
            }
            if self.closed.load(Ordering::SeqCst) || self.fail_writes.load(Ordering::SeqCst) {
                return Err(DuplexError::Closed);
            }
            assert!(data.len() <= DUPLEX_CHUNK);
            self.written.lock().unwrap().push(data);
            Ok(())
        }
        fn shutdown(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.closed.store(true, Ordering::SeqCst);
            let _incoming = self.incoming.lock().unwrap();
            self.wake.notify_all();
            let _held = self.write_gate.lock().unwrap();
            self.write_wake.notify_all();
        }
    }

    async fn until(what: impl Fn() -> bool) {
        for _ in 0..2_000 {
            if what() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("the condition never held");
    }

    #[tokio::test]
    async fn bytes_pass_both_ways() {
        let fake = Arc::new(Fake::default());
        let mut pipe = pipe_from_duplex(fake.clone());
        fake.feed(b"hello ");
        fake.feed(b"there");
        let mut got = [0u8; 11];
        pipe.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello there");
        pipe.write_all(b"and back").await.unwrap();
        until(|| fake.written() == b"and back").await;
    }

    #[tokio::test]
    async fn dropping_the_pipe_closes_the_stream_and_frees_a_blocked_read() {
        let fake = Arc::new(Fake::default());
        let pipe = pipe_from_duplex(fake.clone());
        // The reading thread is parked in the platform's read.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(pipe);
        // Three holders: the fake here and both threads, until they exit.
        until(|| Arc::strong_count(&fake) == 1).await;
        assert!(fake.closes.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn the_end_of_the_stream_reads_as_the_end_and_a_failure_as_an_error() {
        let fake = Arc::new(Fake::default());
        let mut pipe = pipe_from_duplex(fake.clone());
        fake.feed(b"last");
        fake.feed(b"");
        let mut got = Vec::new();
        pipe.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"last");

        let fake = Arc::new(Fake::default());
        let mut pipe = pipe_from_duplex(fake.clone());
        fake.fail_read();
        assert!(pipe.read(&mut [0u8; 8]).await.is_err());
    }

    #[tokio::test]
    async fn a_graceful_shutdown_writes_what_was_queued_first() {
        let fake = Arc::new(Fake::default());
        *fake.write_gate.lock().unwrap() = true;
        let mut pipe = pipe_from_duplex(fake.clone());
        pipe.write_all(&[1u8; 3000]).await.unwrap();
        pipe.write_all(&[2u8; 3000]).await.unwrap();
        let gate = fake.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            *gate.write_gate.lock().unwrap() = false;
            gate.write_wake.notify_all();
        });
        pipe.shutdown().await.unwrap();
        assert_eq!(fake.written().len(), 6000, "the queue was written first");
        assert!(fake.closed.load(Ordering::SeqCst));
        assert!(pipe.write_all(b"late").await.is_err());
    }

    #[tokio::test]
    async fn a_shutdown_does_not_wait_forever_for_a_stuck_stream() {
        let fake = Arc::new(Fake::default());
        *fake.write_gate.lock().unwrap() = true;
        let mut pipe = pipe_from_duplex(fake.clone());
        pipe.write_all(&[1u8; 100]).await.unwrap();
        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(5), pipe.shutdown());
        shutdown.await.expect("bounded").unwrap();
        assert!(fake.closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn more_bytes_than_asked_for_fail_the_pipe() {
        let fake = Arc::new(Fake::default());
        let mut pipe = pipe_from_duplex(fake.clone());
        fake.feed(&vec![0u8; DUPLEX_CHUNK + 1]);
        assert!(pipe.read(&mut [0u8; 8]).await.is_err());
    }

    #[tokio::test]
    async fn a_failed_write_breaks_the_pipe() {
        let fake = Arc::new(Fake::default());
        fake.fail_writes.store(true, Ordering::SeqCst);
        let mut pipe = pipe_from_duplex(fake.clone());
        let mut broke = false;
        for _ in 0..100 {
            if pipe.write_all(&[0u8; 1024]).await.is_err() {
                broke = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(broke);
    }

    #[tokio::test]
    async fn small_writes_are_joined_and_a_slow_stream_pushes_back() {
        let fake = Arc::new(Fake::default());
        *fake.write_gate.lock().unwrap() = true;
        let mut pipe = pipe_from_duplex(fake.clone());
        // The platform takes nothing: only the small queue fills.
        let mut accepted = 0usize;
        loop {
            let write = pipe.write_all(&[7u8; 1000]);
            match tokio::time::timeout(std::time::Duration::from_millis(50), write).await {
                Ok(result) => {
                    result.unwrap();
                    accepted += 1;
                }
                Err(_) => break,
            }
            assert!(accepted < 100, "nothing pushed back");
        }
        *fake.write_gate.lock().unwrap() = false;
        fake.write_wake.notify_all();
        until(|| fake.written().len() >= accepted * 1000).await;
        let calls = fake.written.lock().unwrap().len();
        assert!(calls <= accepted, "{calls} calls for {accepted} writes");
    }
}
