//! The public-certificate/OPAQUE bootstrap socket accepts no application commands.
//! A trusted pin is released only after proof verification and native persistence.
use crate::{
    identity::{PeerIdentity, certificate_fingerprint},
    pairing::{PairingClient, PairingWindow},
    runtime,
};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, mpsc, oneshot, watch},
    task::JoinSet,
};

const MAX_BYTES: usize = 16_384;
const WINDOW_LIFETIME: Duration = Duration::from_secs(120);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CONNECTIONS: usize = 8;
const MAX_PATHS: usize = 16;
const PATH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PairingNetworkError {
    #[error("invalid pairing address or message")]
    InvalidMessage,
    #[error("pairing authentication failed")]
    Authentication,
    #[error("pairing invitation names a different host")]
    WrongHost,
    #[error("host could not save this pairing")]
    Persistence,
    #[error("pairing service has closed or expired")]
    Closed,
    #[error("pairing exchange timed out")]
    Timeout,
    #[error("pairing network operation failed")]
    Network,
    #[error("pairing runtime is unavailable")]
    Unavailable,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Hello {
        major: u16,
        certificate: Vec<u8>,
    },
    Request {
        certificate: Vec<u8>,
        request: Vec<u8>,
    },
    Challenge {
        attempt: u32,
        response: Vec<u8>,
    },
    Finish {
        attempt: u32,
        proof: Vec<u8>,
    },
    Persisted {
        confirmation: Vec<u8>,
    },
    Failed,
}

async fn read(stream: &mut TcpStream) -> Result<Message, PairingNetworkError> {
    let size = stream
        .read_u32()
        .await
        .map_err(|_| PairingNetworkError::Network)? as usize;
    if size == 0 || size > MAX_BYTES {
        return Err(PairingNetworkError::InvalidMessage);
    }
    let mut bytes = vec![0; size];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| PairingNetworkError::Network)?;
    serde_json::from_slice(&bytes).map_err(|_| PairingNetworkError::InvalidMessage)
}

async fn write(stream: &mut TcpStream, message: &Message) -> Result<(), PairingNetworkError> {
    let bytes = serde_json::to_vec(message).map_err(|_| PairingNetworkError::InvalidMessage)?;
    if bytes.len() > MAX_BYTES {
        return Err(PairingNetworkError::InvalidMessage);
    }
    stream
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| PairingNetworkError::Network)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|_| PairingNetworkError::Network)?;
    Ok(())
}

/// The native host must write this certificate successfully to its credential
/// store before calling confirm_persisted. Dropping/rejecting it sends no proof.
#[derive(uniffi::Object)]
pub struct PendingPairing {
    certificate: Vec<u8>,
    persisted: Mutex<Option<oneshot::Sender<bool>>>,
    delivered: Mutex<Option<oneshot::Receiver<Result<(), PairingNetworkError>>>>,
    closed: watch::Sender<bool>,
}

#[uniffi::export]
impl PendingPairing {
    pub fn peer_certificate(&self) -> Vec<u8> {
        self.certificate.clone()
    }
    pub async fn confirm_persisted(&self) -> Result<(), PairingNetworkError> {
        self.complete(true)?;
        let delivered = self
            .delivered
            .lock()
            .map_err(|_| PairingNetworkError::Unavailable)?
            .take()
            .ok_or(PairingNetworkError::Closed)?;
        runtime::run(async move { delivered.await.map_err(|_| PairingNetworkError::Closed)? })
            .await
            .map_err(|_| PairingNetworkError::Unavailable)?
    }
    pub fn reject_persistence(&self) -> Result<(), PairingNetworkError> {
        self.complete(false)
    }
}

impl PendingPairing {
    fn complete(&self, success: bool) -> Result<(), PairingNetworkError> {
        if *self.closed.borrow() {
            return Err(PairingNetworkError::Closed);
        }
        self.persisted
            .lock()
            .map_err(|_| PairingNetworkError::Unavailable)?
            .take()
            .ok_or(PairingNetworkError::Closed)?
            .send(success)
            .map_err(|_| PairingNetworkError::Closed)
    }
}

#[derive(uniffi::Object)]
pub struct PairingService {
    window: Arc<PairingWindow>,
    address: String,
    closed: watch::Sender<bool>,
    pending: Arc<AsyncMutex<mpsc::Receiver<Arc<PendingPairing>>>>,
    task: tokio::task::AbortHandle,
    completion: Arc<AsyncMutex<Option<tokio::task::JoinHandle<()>>>>,
}

#[uniffi::export]
impl PairingService {
    pub fn address(&self) -> String {
        self.address.clone()
    }
    pub fn display_code(&self) -> Result<String, PairingNetworkError> {
        if *self.closed.borrow() {
            return Err(PairingNetworkError::Closed);
        }
        self.window
            .display_code()
            .map_err(|_| PairingNetworkError::Closed)
    }
    pub fn close(&self) {
        self.closed.send_replace(true);
        let _ = self.window.cancel();
        self.task.abort();
    }
    /// Wait until the listener and its exchanges have been dropped before
    /// rebinding the pairing port. Concurrent callers share the same barrier.
    pub async fn close_and_wait(&self) -> Result<(), PairingNetworkError> {
        self.close();
        let completion = self.completion.clone();
        runtime::run(async move {
            let mut completion = completion.lock().await;
            if let Some(task) = completion.as_mut() {
                let _ = task.await;
            }
            *completion = None;
        })
        .await
        .map_err(|_| PairingNetworkError::Unavailable)
    }
    pub async fn next_verified(&self) -> Result<Arc<PendingPairing>, PairingNetworkError> {
        let receiver = self.pending.clone();
        let mut closed = self.closed.subscribe();
        runtime::run(async move {
            if *closed.borrow() { return Err(PairingNetworkError::Closed); }
            tokio::select! {
                biased;
                _ = closed.changed() => Err(PairingNetworkError::Closed),
                pending = async { receiver.lock().await.recv().await } => pending.ok_or(PairingNetworkError::Closed),
            }
        }).await.map_err(|_| PairingNetworkError::Unavailable)?
    }
}

impl Drop for PairingService {
    fn drop(&mut self) {
        self.close();
    }
}

struct CloseWindow {
    window: Arc<PairingWindow>,
    closed: watch::Sender<bool>,
}
impl Drop for CloseWindow {
    fn drop(&mut self) {
        self.closed.send_replace(true);
        let _ = self.window.cancel();
    }
}

#[uniffi::export]
pub async fn listen_pairing(
    address: String,
    identity: Arc<PeerIdentity>,
) -> Result<Arc<PairingService>, PairingNetworkError> {
    open_service(address, identity, WINDOW_LIFETIME).await
}

async fn open_service(
    address: String,
    identity: Arc<PeerIdentity>,
    lifetime: Duration,
) -> Result<Arc<PairingService>, PairingNetworkError> {
    runtime::run(async move {
        let window = Arc::new(
            tokio::task::spawn_blocking(move || PairingWindow::new(identity.certificate()))
                .await
                .map_err(|_| PairingNetworkError::Unavailable)?
                .map_err(|_| PairingNetworkError::InvalidMessage)?,
        );
        let listener = TcpListener::bind(
            address
                .parse::<SocketAddr>()
                .map_err(|_| PairingNetworkError::InvalidMessage)?,
        )
        .await
        .map_err(|_| PairingNetworkError::Network)?;
        let address = listener
            .local_addr()
            .map_err(|_| PairingNetworkError::Network)?
            .to_string();
        let closed = watch::channel(false).0;
        let (sender, receiver) = mpsc::channel(1);
        let guard = CloseWindow {
            window: window.clone(),
            closed: closed.clone(),
        };
        let task = tokio::spawn(async move {
            let _ = tokio::time::timeout(lifetime, serve(listener, &guard, sender)).await;
        });
        Ok(Arc::new(PairingService {
            window,
            address,
            closed,
            pending: Arc::new(AsyncMutex::new(receiver)),
            task: task.abort_handle(),
            completion: Arc::new(AsyncMutex::new(Some(task))),
        }))
    })
    .await
    .map_err(|_| PairingNetworkError::Unavailable)?
}

async fn serve(
    listener: TcpListener,
    guard: &CloseWindow,
    sender: mpsc::Sender<Arc<PendingPairing>>,
) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if tasks.len() < MAX_CONNECTIONS => {
                let Ok((stream, _)) = accepted else { break };
                let window = guard.window.clone();
                let closed = guard.closed.clone();
                let sender = sender.clone();
                tasks.spawn(async move {
                    let _ = tokio::time::timeout(EXCHANGE_TIMEOUT, accept_pairing(stream, window, closed, sender)).await;
                });
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
}

async fn accept_pairing(
    mut stream: TcpStream,
    window: Arc<PairingWindow>,
    closed: watch::Sender<bool>,
    sender: mpsc::Sender<Arc<PendingPairing>>,
) -> Result<(), PairingNetworkError> {
    // Only public data goes over this bootstrap socket. The PIN authenticates
    // the certificate hashes inside OPAQUE, including any invitation pin.
    let certificate = window.host_certificate();
    write(
        &mut stream,
        &Message::Hello {
            major: 1,
            certificate,
        },
    )
    .await?;
    let request = read(&mut stream).await;
    let (certificate, request) = match request {
        Ok(Message::Request {
            certificate,
            request,
        }) => (certificate, request),
        // Path races and cancelled clients can disconnect after reading the
        // public hello without submitting a PIN proof. They spend no attempt.
        Err(PairingNetworkError::Network) => return Err(PairingNetworkError::Network),
        _ => {
            let _ = window.begin(vec![], vec![]);
            return Err(PairingNetworkError::InvalidMessage);
        }
    };
    let challenge = window
        .begin(certificate, request)
        .map_err(|_| PairingNetworkError::Authentication)?;
    write(
        &mut stream,
        &Message::Challenge {
            attempt: challenge.attempt,
            response: challenge.response,
        },
    )
    .await?;
    let Message::Finish { attempt, proof } = read(&mut stream).await? else {
        return Err(PairingNetworkError::InvalidMessage);
    };
    // A socket may finish only the attempt that it began.
    if attempt != challenge.attempt {
        return Err(PairingNetworkError::Authentication);
    }
    let verified = window
        .finish(attempt, proof)
        .map_err(|_| PairingNetworkError::Authentication)?;
    let (persisted, result) = oneshot::channel();
    let (delivered, delivery) = oneshot::channel();
    let pending = Arc::new(PendingPairing {
        certificate: verified.peer_certificate,
        persisted: Mutex::new(Some(persisted)),
        delivered: Mutex::new(Some(delivery)),
        closed,
    });
    sender
        .send(pending)
        .await
        .map_err(|_| PairingNetworkError::Closed)?;
    if !result.await.unwrap_or(false) {
        write(&mut stream, &Message::Failed).await?;
        return Err(PairingNetworkError::Persistence);
    }
    let result = write(
        &mut stream,
        &Message::Persisted {
            confirmation: verified.persisted_confirmation,
        },
    )
    .await;
    let _ = delivered.send(result);
    Ok(())
}

/// Returns an authenticated host certificate only after the host acknowledges
/// persistence. The caller must save it locally before reporting paired.
/// Only the winning public-hello path receives a PAKE request. Authentication
/// or persistence failure never resubmits the code to another address.
#[uniffi::export]
pub async fn pair_with_host(
    addresses: Vec<String>,
    identity: Arc<PeerIdentity>,
    code: String,
    invitation_fingerprint: Option<String>,
    cancellation: Arc<crate::OperationCancellation>,
) -> Result<Vec<u8>, PairingNetworkError> {
    let code = zeroize::Zeroizing::new(code);
    runtime::run(async move {
        cancellation
            .run(tokio::time::timeout(EXCHANGE_TIMEOUT, async move {
                let (mut stream, certificate) =
                    pairing_path(addresses, invitation_fingerprint).await?;
                let pair = Arc::new(
                    PairingClient::new(code.to_string(), certificate, identity.certificate())
                        .map_err(|_| PairingNetworkError::Authentication)?,
                );
                write(
                    &mut stream,
                    &Message::Request {
                        certificate: identity.certificate(),
                        request: pair.request(),
                    },
                )
                .await?;
                let Message::Challenge { attempt, response } = read(&mut stream).await? else {
                    return Err(PairingNetworkError::Authentication);
                };
                let completing = pair.clone();
                let proof = tokio::task::spawn_blocking(move || completing.finish(response))
                    .await
                    .map_err(|_| PairingNetworkError::Unavailable)?
                    .map_err(|_| PairingNetworkError::Authentication)?;
                write(&mut stream, &Message::Finish { attempt, proof }).await?;
                let Message::Persisted { confirmation } = read(&mut stream).await? else {
                    return Err(PairingNetworkError::Persistence);
                };
                pair.confirm_persisted(confirmation)
                    .map_err(|_| PairingNetworkError::Authentication)
            }))
            .await
            .ok_or(PairingNetworkError::Closed)?
            .map_err(|_| PairingNetworkError::Timeout)?
    })
    .await
    .map_err(|_| PairingNetworkError::Unavailable)?
}

async fn pairing_path(
    addresses: Vec<String>,
    invitation_fingerprint: Option<String>,
) -> Result<(TcpStream, Vec<u8>), PairingNetworkError> {
    if addresses.is_empty() || addresses.len() > MAX_PATHS {
        return Err(PairingNetworkError::InvalidMessage);
    }
    let mut paths = Vec::new();
    for address in addresses {
        let address: SocketAddr = address
            .parse()
            .map_err(|_| PairingNetworkError::InvalidMessage)?;
        if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(PairingNetworkError::InvalidMessage);
        }
        if !paths.contains(&address) {
            paths.push(address);
        }
    }
    let mut attempts = JoinSet::new();
    for (index, address) in paths.into_iter().enumerate() {
        let expected = invitation_fingerprint.clone();
        attempts.spawn(async move {
            tokio::time::sleep(Duration::from_millis(index as u64 * 100)).await;
            tokio::time::timeout(PATH_TIMEOUT, async move {
                let mut stream = TcpStream::connect(address)
                    .await
                    .map_err(|_| PairingNetworkError::Network)?;
                let Message::Hello {
                    major: 1,
                    certificate,
                } = read(&mut stream).await?
                else {
                    return Err(PairingNetworkError::InvalidMessage);
                };
                if expected.is_some_and(|pin| pin != certificate_fingerprint(&certificate)) {
                    return Err(PairingNetworkError::WrongHost);
                }
                Ok((stream, certificate))
            })
            .await
            .map_err(|_| PairingNetworkError::Timeout)?
        });
    }
    let mut failure = PairingNetworkError::Network;
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok(winner)) => {
                // Abort and drain before any proof is generated. Any other
                // completed sockets drop here, so none can consume a code.
                attempts.abort_all();
                while attempts.join_next().await.is_some() {}
                return Ok(winner);
            }
            Ok(Err(PairingNetworkError::WrongHost)) => failure = PairingNetworkError::WrongHost,
            Ok(Err(error)) if !matches!(failure, PairingNetworkError::WrongHost) => failure = error,
            _ => {}
        }
    }
    Err(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn service_expiry_closes_the_listener_and_pending_native_wait() {
        let identity = Arc::new(PeerIdentity::generate().unwrap());
        let service = open_service("127.0.0.1:0".into(), identity, Duration::from_millis(20))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), service.next_verified())
                .await
                .unwrap()
                .is_err()
        );
        assert!(service.display_code().is_err());
        assert!(TcpStream::connect(service.address()).await.is_err());
    }
}
