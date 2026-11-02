//! Manual two-machine QA probe. Only public certificates cross the shared test
//! directory; ephemeral private identities stay in memory. This is not a daemon.
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use streamcore_transport::{
    OperationCancellation,
    channel::{
        ChannelPath, ChannelTransport, SecureChannel, SecureListener, SessionOptions,
        connect_secure, listen_secure,
    },
    identity::PeerIdentity,
    multiplex::ChannelLane,
};
use tokio::task::JoinSet;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    certificate: Vec<u8>,
    port: u16,
}

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec!["probe.echo.v2".into()],
        required: vec!["probe.echo.v2".into()],
    }
}

const LANES: [ChannelLane; 5] = [
    ChannelLane::Interactive,
    ChannelLane::Video,
    ChannelLane::Audio,
    ChannelLane::Terminal,
    ChannelLane::Bulk,
];

async fn echo(listener: Arc<SecureListener>) {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if sessions.len() < 16 => {
                if let Ok(session) = accepted {
                    sessions.spawn(async move {
                        while let Ok(bytes) = session.receive().await {
                            let Some(lane) = bytes.first().and_then(|n| LANES.get(*n as usize)) else { break; };
                            let Ok(receipt) = session.enqueue(*lane, bytes) else { break; };
                            if receipt.written().await.is_err() { break; }
                        }
                    });
                } else {
                    break;
                }
            }
            _ = sessions.join_next(), if !sessions.is_empty() => {},
        }
    }
}

async fn server(directory: &Path) -> Result<()> {
    let client: Vec<u8> =
        serde_json::from_slice(&std::fs::read(directory.join("client-public.json"))?)?;
    let identity = Arc::new(PeerIdentity::generate()?);
    let listener = listen_secure(
        "0.0.0.0:0".into(),
        identity.clone(),
        vec![client],
        options(),
    )
    .await?;
    let manifest = Manifest {
        certificate: identity.certificate(),
        port: listener.address().parse::<SocketAddr>()?.port(),
    };
    // Rename avoids exposing a partially written manifest to the client.
    std::fs::write(
        directory.join("server-public.tmp"),
        serde_json::to_vec(&manifest)?,
    )?;
    std::fs::rename(
        directory.join("server-public.tmp"),
        directory.join("server-public.json"),
    )?;
    println!("Probe server ready; only the preselected test client is trusted.");
    // The diagnostic listener cannot remain running indefinitely after a failure.
    let _ = tokio::time::timeout(Duration::from_secs(180), echo(listener.clone())).await;
    listener.close();
    Ok(())
}

async fn connect(
    identity: &Arc<PeerIdentity>,
    manifest: &Manifest,
    ip: std::net::IpAddr,
    transport: ChannelTransport,
) -> Result<Arc<SecureChannel>> {
    Ok(connect_secure(
        identity.clone(),
        manifest.certificate.clone(),
        vec![ChannelPath {
            address: SocketAddr::new(ip, manifest.port).to_string(),
            transport,
            delay_ms: 0,
        }],
        options(),
        OperationCancellation::new(),
    )
    .await?)
}

async fn exchange(session: &SecureChannel, sequence: u32) -> Result<()> {
    let mut expected = Vec::new();
    let mut sends = Vec::new();
    for (id, lane) in LANES.iter().enumerate() {
        let mut payload = vec![0x5a; 16_384];
        payload[0] = id as u8;
        payload[1..5].copy_from_slice(&sequence.to_be_bytes());
        sends.push(session.enqueue(*lane, payload.clone())?);
        expected.push(payload);
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        for receipt in sends {
            receipt.written().await?;
        }
        for _ in 0..LANES.len() {
            let response = session.receive().await?;
            let Some(index) = expected.iter().position(|p| *p == response) else {
                return Err("response bytes differ or duplicate lane".into());
            };
            expected.swap_remove(index);
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await??;
    Ok(())
}

async fn client(directory: &Path, remote_ip: std::net::IpAddr) -> Result<()> {
    let identity = Arc::new(PeerIdentity::generate()?);
    std::fs::write(
        directory.join("client-public.json"),
        serde_json::to_vec(&identity.certificate())?,
    )?;
    println!("Client public certificate ready. Start the VM probe server now.");
    let remote: Manifest = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            match std::fs::read(directory.join("server-public.json")) {
                Ok(data) => return serde_json::from_slice(&data),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(100)).await
                }
                Err(error) => return Err(serde_json::Error::io(error)),
            }
        }
    })
    .await??;
    let local_identity = Arc::new(PeerIdentity::generate()?);
    let local_listener = listen_secure(
        "127.0.0.1:0".into(),
        local_identity.clone(),
        vec![identity.certificate()],
        options(),
    )
    .await?;
    let local = Manifest {
        certificate: local_identity.certificate(),
        port: local_listener.address().parse::<SocketAddr>()?.port(),
    };
    let server = tokio::spawn(echo(local_listener.clone()));
    // Abort the local endpoint on errors as well as on the intentional stop.
    struct Stop(tokio::task::AbortHandle);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop = Stop(server.abort_handle());
    let mut local_sessions = Vec::new();
    let mut remote_sessions = Vec::new();
    for (label, kind) in [
        ("QUIC", ChannelTransport::Quic),
        ("TLS/TCP", ChannelTransport::TlsTcp),
    ] {
        let started = Instant::now();
        let remote_session = connect(&identity, &remote, remote_ip, kind).await?;
        println!(
            "VM {label}: authenticated and negotiated in {} ms",
            started.elapsed().as_millis()
        );
        let local_session = connect(&identity, &local, "127.0.0.1".parse()?, kind).await?;
        if local_session.peer_fingerprint() == remote_session.peer_fingerprint() {
            return Err("endpoint identities collided".into());
        }
        local_sessions.push(local_session);
        remote_sessions.push(remote_session);
    }
    for sequence in 0..100_u32 {
        for session in local_sessions.iter().chain(remote_sessions.iter()) {
            exchange(session, sequence).await?;
        }
    }
    println!(
        "100 verified 16-KiB exchanges per lane on each of four simultaneous channels passed."
    );
    local_listener.close();
    server.abort();
    let _ = server.await;
    for session in &local_sessions {
        if tokio::time::timeout(Duration::from_secs(3), session.receive())
            .await?
            .is_ok()
        {
            return Err("stopped endpoint remained active".into());
        }
    }
    for sequence in 100..200 {
        for session in &remote_sessions {
            exchange(session, sequence).await?;
        }
    }
    remote_sessions[0].rebind("0.0.0.0:0".into()).await?;
    exchange(&remote_sessions[0], 200).await?;
    println!(
        "PASS: VM QUIC/TCP survived the local endpoint stop; QUIC socket rebind retained the peer."
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.as_slice() {
        [_, mode, directory] if mode == "server" => server(Path::new(directory)).await,
        [_, mode, directory, remote] if mode == "client" => client(Path::new(directory), remote.parse()?).await,
        _ => Err("usage: network_probe server DIRECTORY | client DIRECTORY VM_IP; use a fresh test directory".into()),
    }
}
