//! Opt-in transport scheduling measurement, not capture/display qualification.
//! Only public certificates cross disk; private identities remain in RAM.
use serde::{Deserialize, Serialize};
use std::{error::Error, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use streamcore_transport::{
    OperationCancellation,
    channel::{
        ChannelError, ChannelPath, ChannelTransport, SecureChannel, SecureListener, SessionOptions,
        connect_secure, listen_secure,
    },
    identity::PeerIdentity,
    multiplex::ChannelLane,
};
use tokio::{task::JoinSet, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const LANES: [ChannelLane; 5] = [
    ChannelLane::Interactive,
    ChannelLane::Video,
    ChannelLane::Audio,
    ChannelLane::Terminal,
    ChannelLane::Bulk,
];
const NAMES: [&str; 5] = ["interactive", "video", "audio", "terminal", "bulk"];
const RATES: [u64; 5] = [60, 120, 100, 20, 32];
const SIZES: [usize; 5] = [64, 96 * 1024, 1024, 4096, 1024 * 1024];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    certificate: Vec<u8>,
    port: u16,
}

fn options() -> SessionOptions {
    SessionOptions {
        capabilities: vec!["probe.scheduling.v1".into()],
        required: vec!["probe.scheduling.v1".into()],
    }
}

fn publish<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

async fn echo(session: Arc<SecureChannel>) -> Result<()> {
    // Never await one lane's write before reading another lane. Receipt tasks
    // are bounded; transport admission remains authoritative under pressure.
    let mut writes = JoinSet::new();
    // SecureChannel intentionally closes when a pending receive is cancelled.
    // Keep the same receive future alive while servicing write completions.
    let mut incoming = Box::pin(session.receive());
    loop {
        tokio::select! {
            result = writes.join_next(), if !writes.is_empty() => { result.ok_or("missing writer")???; }
            result = &mut incoming, if writes.len() < 128 => {
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(ChannelError::Closed) => return Ok(()),
                    Err(error) => return Err(error.into()),
                };
                let lane = *LANES.get(usize::from(*bytes.first().ok_or("empty probe")?))
                    .ok_or("invalid lane")?;
                let receipt = session.enqueue(lane, bytes)?;
                writes.spawn(async move { receipt.written().await });
                incoming = Box::pin(session.receive());
            }
        }
    }
}

async fn serve(listener: Arc<SecureListener>) -> Result<()> {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            result = sessions.join_next(), if !sessions.is_empty() => {
                // Peer closure/protocol failure is scoped to that connection.
                // Ending the listener here can cancel a later measured peer.
                match result.ok_or("missing session")?? {
                    Ok(()) => {},
                    Err(error) => eprintln!("probe peer ended: {error}"),
                }
            }
            session = listener.accept(), if sessions.len() < 2 => { sessions.spawn(echo(session?)); }
        }
    }
}

#[derive(Default, Serialize)]
struct Distribution {
    samples: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}
fn distribution(mut nanos: Vec<u64>) -> Distribution {
    nanos.sort_unstable();
    if nanos.is_empty() {
        return Distribution::default();
    }
    let n = nanos.len();
    let percentile = |percent: usize| nanos[(n * percent).div_ceil(100) - 1] as f64 / 1_000_000.0;
    Distribution {
        samples: n,
        p50_ms: percentile(50),
        p95_ms: percentile(95),
        p99_ms: percentile(99),
        max_ms: nanos[n - 1] as f64 / 1_000_000.0,
    }
}

#[derive(Serialize)]
struct Offered {
    lane: &'static str,
    rate_per_second: u64,
    bytes_per_message: usize,
    scheduled: u64,
    missed_deadlines: u64,
    backpressured: u64,
    accepted: u64,
    send_lateness: Distribution,
    write_completion: Distribution,
}

async fn produce(
    session: Arc<SecureChannel>,
    start: Instant,
    seconds: u64,
    lane: usize,
    rate: u64,
    size: usize,
) -> Result<Offered> {
    let scheduled = seconds * rate;
    let mut stats = Offered {
        lane: NAMES[lane],
        rate_per_second: rate,
        bytes_per_message: size,
        scheduled,
        missed_deadlines: 0,
        backpressured: 0,
        accepted: 0,
        send_lateness: Distribution::default(),
        write_completion: Distribution::default(),
    };
    let template = vec![0xa0 + lane as u8; size];
    let mut lateness = Vec::new();
    let mut written = Vec::new();
    let mut writes = JoinSet::new();
    let mut slot = 0;
    while slot < scheduled {
        let deadline = start + Duration::from_nanos(slot * 1_000_000_000 / rate);
        tokio::time::sleep_until(deadline).await;
        let elapsed = start.elapsed().as_nanos() as u64;
        let current_slot = (elapsed * rate / 1_000_000_000).min(scheduled);
        if current_slot > slot {
            stats.missed_deadlines += current_slot - slot;
            slot = current_slot;
        }
        if slot == scheduled {
            break;
        }
        while let Some(result) = writes.try_join_next() {
            written.push(result??);
        }
        let mut bytes = template.clone();
        bytes[0] = lane as u8;
        bytes[1..9].copy_from_slice(&stats.accepted.to_be_bytes());
        let sent = start.elapsed().as_nanos() as u64;
        bytes[9..17].copy_from_slice(&sent.to_be_bytes());
        lateness.push(sent.saturating_sub(slot * 1_000_000_000 / rate));
        match session.enqueue(LANES[lane], bytes) {
            Ok(receipt) => {
                stats.accepted += 1;
                writes.spawn(async move {
                    receipt.written().await?;
                    Ok::<_, ChannelError>(start.elapsed().as_nanos() as u64 - sent)
                });
            }
            Err(ChannelError::Backpressure) => stats.backpressured += 1,
            Err(error) => return Err(error.into()),
        }
        slot += 1;
    }
    while let Some(result) = writes.join_next().await {
        written.push(result??);
    }
    stats.write_completion = distribution(written);
    stats.send_lateness = distribution(lateness);
    Ok(stats)
}

#[derive(Default)]
struct Received {
    counts: [u64; 5],
    rtt: [Vec<u64>; 5],
    gaps: [Vec<u64>; 5],
    previous: [Option<u64>; 5],
}
async fn consume(
    session: Arc<SecureChannel>,
    start: Instant,
    mut expected: tokio::sync::watch::Receiver<Option<[u64; 5]>>,
    bulk_size: usize,
) -> Result<Received> {
    let mut state = Received::default();
    let mut incoming = Box::pin(session.receive());
    loop {
        if expected.borrow().as_ref() == Some(&state.counts) {
            return Ok(state);
        }
        tokio::select! {
            update = expected.changed(), if expected.borrow().is_none() => { update?; }
            bytes = &mut incoming => {
                let bytes = bytes?;
                let lane = usize::from(*bytes.first().ok_or("empty reply")?);
                if lane >= LANES.len() || bytes.len() != if lane == 4 { bulk_size } else { SIZES[lane] } { return Err("wrong reply size/lane".into()); }
                let sequence = u64::from_be_bytes(bytes[1..9].try_into()?);
                let sent = u64::from_be_bytes(bytes[9..17].try_into()?);
                let now = start.elapsed().as_nanos() as u64;
                if sequence != state.counts[lane] || sent > now
                    || bytes[17..].iter().any(|byte| *byte != 0xa0 + lane as u8) {
                    return Err("corrupt, reordered or duplicated reply".into());
                }
                state.counts[lane] += 1;
                state.rtt[lane].push(now - sent);
                if let Some(previous) = state.previous[lane].replace(now) {
                    state.gaps[lane].push(now - previous);
                }
                incoming = Box::pin(session.receive());
            }
        }
    }
}

#[derive(Serialize)]
struct LaneReport {
    offered: Offered,
    received: u64,
    round_trip: Distribution,
    receive_gap: Distribution,
}
#[derive(Serialize)]
struct Report {
    transport: &'static str,
    remote: String,
    seconds: u64,
    bulk_messages_per_second: u64,
    handshake_ms: f64,
    elapsed_ms: f64,
    lanes: Vec<LaneReport>,
}

async fn measure(
    identity: Arc<PeerIdentity>,
    remote: &Manifest,
    ip: std::net::IpAddr,
    transport: ChannelTransport,
    seconds: u64,
    bulk_rate: u64,
    bulk_size: usize,
) -> Result<Report> {
    let connected = Instant::now();
    let address = SocketAddr::new(ip, remote.port).to_string();
    let channel = connect_secure(
        identity,
        remote.certificate.clone(),
        vec![ChannelPath {
            address: address.clone(),
            transport,
            delay_ms: 0,
        }],
        options(),
        OperationCancellation::new(),
    )
    .await?;
    struct Close(Arc<SecureChannel>);
    impl Drop for Close {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close = Close(channel.clone());
    let handshake_ms = connected.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now() + Duration::from_millis(100);
    let (expected_tx, expected_rx) = tokio::sync::watch::channel(None);
    let send = async {
        let mut producers = JoinSet::new();
        for lane in 0..LANES.len() {
            producers.spawn(produce(
                channel.clone(),
                start,
                seconds,
                lane,
                if lane == 4 { bulk_rate } else { RATES[lane] },
                if lane == 4 { bulk_size } else { SIZES[lane] },
            ));
        }
        let mut offers = Vec::new();
        while let Some(result) = producers.join_next().await {
            offers.push(result??);
        }
        offers.sort_by_key(|offer| NAMES.iter().position(|name| *name == offer.lane));
        expected_tx.send(Some(std::array::from_fn(|lane| offers[lane].accepted)))?;
        Ok::<_, Box<dyn Error + Send + Sync>>(offers)
    };
    let (offers, mut received) = tokio::time::timeout(Duration::from_secs(seconds + 15), async {
        tokio::try_join!(
            send,
            consume(channel.clone(), start, expected_rx, bulk_size)
        )
    })
    .await??;
    let lanes = offers
        .into_iter()
        .enumerate()
        .map(|(lane, offered)| LaneReport {
            offered,
            received: received.counts[lane],
            round_trip: distribution(std::mem::take(&mut received.rtt[lane])),
            receive_gap: distribution(std::mem::take(&mut received.gaps[lane])),
        })
        .collect();
    Ok(Report {
        transport: match transport {
            ChannelTransport::Quic => "quic",
            ChannelTransport::TlsTcp => "tls_tcp",
        },
        remote: address,
        seconds,
        bulk_messages_per_second: bulk_rate,
        handshake_ms,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        lanes,
    })
}

async fn listener(
    identity: Arc<PeerIdentity>,
    client: Vec<u8>,
    address: &str,
) -> Result<Arc<SecureListener>> {
    Ok(listen_secure(address.into(), identity, vec![client], options()).await?)
}

async fn server(directory: &Path) -> Result<()> {
    let client = serde_json::from_slice(&std::fs::read(directory.join("client-public.json"))?)?;
    let identity = Arc::new(PeerIdentity::generate()?);
    let listener = listener(identity.clone(), client, "0.0.0.0:0").await?;
    publish(
        &directory.join("server-public.json"),
        &Manifest {
            certificate: identity.certificate(),
            port: listener.address().parse::<SocketAddr>()?.port(),
        },
    )?;
    println!("Ready: pinned test peer only; expires after 180 seconds");
    let result = tokio::time::timeout(Duration::from_secs(180), serve(listener.clone())).await;
    listener.close();
    match result {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

async fn client(
    directory: &Path,
    ip: std::net::IpAddr,
    seconds: u64,
    bulk_rate: u64,
    bulk_size: usize,
) -> Result<()> {
    if ![262144, 1048576].contains(&bulk_size) || bulk_rate > 128 {
        return Err("bulk rate must be 0–128 messages/s and size 262144 or 1048576 bytes".into());
    }
    if !(1..=60).contains(&seconds) {
        return Err("duration must be 1–60 seconds per transport".into());
    }
    let identity = Arc::new(PeerIdentity::generate()?);
    publish(
        &directory.join("client-public.json"),
        &identity.certificate(),
    )?;
    let manifest: Manifest = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match std::fs::read(directory.join("server-public.json")) {
                Ok(data) => return serde_json::from_slice(&data),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(error) => return Err(serde_json::Error::io(error)),
            }
        }
    })
    .await??;
    let mut reports = Vec::new();
    for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
        reports.push(
            measure(
                identity.clone(),
                &manifest,
                ip,
                transport,
                seconds,
                bulk_rate,
                bulk_size,
            )
            .await?,
        );
    }
    publish(&directory.join("report.json"), &reports)?;
    println!("Measurement complete: report.json (not a capture/display latency result)");
    Ok(())
}

async fn self_test() -> Result<()> {
    let peer = Arc::new(PeerIdentity::generate()?);
    let host = Arc::new(PeerIdentity::generate()?);
    let listener = listener(host.clone(), peer.certificate(), "127.0.0.1:0").await?;
    let manifest = Manifest {
        certificate: host.certificate(),
        port: listener.address().parse::<SocketAddr>()?.port(),
    };
    let mut server = JoinSet::new();
    server.spawn(serve(listener.clone()));
    // A peer-level protocol failure must not end the server for other peers.
    let malformed = connect_secure(
        peer.clone(),
        manifest.certificate.clone(),
        vec![ChannelPath {
            address: format!("127.0.0.1:{}", manifest.port),
            transport: ChannelTransport::TlsTcp,
            delay_ms: 0,
        }],
        options(),
        OperationCancellation::new(),
    )
    .await?;
    malformed
        .enqueue(ChannelLane::Interactive, vec![255])?
        .written()
        .await?;
    let rejected = tokio::time::timeout(Duration::from_secs(5), malformed.receive()).await?;
    malformed.close();
    if !matches!(rejected, Err(ChannelError::Closed)) {
        return Err("malformed probe peer was not closed".into());
    }
    if let Some(result) = server.try_join_next() {
        return Err(format!("echo server stopped after malformed peer: {result:?}").into());
    }
    for (bulk_rate, bulk_size) in [(0, 1048576), (32, 1048576), (128, 262144)] {
        for transport in [ChannelTransport::Quic, ChannelTransport::TlsTcp] {
            let report = measure(
                peer.clone(),
                &manifest,
                "127.0.0.1".parse()?,
                transport,
                2,
                bulk_rate,
                bulk_size,
            )
            .await?;
            for lane in &report.lanes {
                if (lane.received == 0 && lane.offered.scheduled != 0)
                    || lane.offered.write_completion.samples as u64 != lane.received
                    || lane.received != lane.offered.accepted
                    || lane.offered.scheduled
                        != lane.offered.accepted
                            + lane.offered.backpressured
                            + lane.offered.missed_deadlines
                {
                    return Err("probe failed count/integrity self-check".into());
                }
            }
            if report.lanes[4].offered.rate_per_second != bulk_rate
                || report.lanes[4].offered.bytes_per_message != bulk_size
            {
                return Err("probe did not use the requested bulk workload".into());
            }
            println!(
                "{} (bulk {bulk_rate}/s): all lane accounting verified",
                report.transport
            );
        }
    }
    listener.close();
    server.abort_all();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.as_slice() {
        [_, mode] if mode == "self-test" => self_test().await,
        [_, mode, directory] if mode == "server" => server(Path::new(directory)).await,
        [_, mode, directory, ip, seconds, bulk, size] if mode == "client" => client(Path::new(directory), ip.parse()?, seconds.parse()?, bulk.parse()?, size.parse()?).await,
        _ => Err("usage: scheduling_probe self-test | server FRESH_DIRECTORY | client FRESH_DIRECTORY IP SECONDS_PER_TRANSPORT BULK_MESSAGES_PER_SECOND BULK_BYTES".into()),
    }
}
