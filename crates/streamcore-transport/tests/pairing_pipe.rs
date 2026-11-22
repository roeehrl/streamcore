//! Pairing over a pipe a USB provider opened (STREAMCORE-VIDEO 8.5): the same
//! exchange as on the pairing socket, with the same code, attempts and
//! persistence contract, only while the host's window is open.

use std::sync::Arc;
use streamcore_transport::{
    OperationCancellation,
    identity::{PeerIdentity, certificate_fingerprint},
    pairing_transport::{
        PairingNetworkError, PairingService, listen_pairing, pair_with_host, pair_with_host_stream,
    },
    transport::{PipeIo, PipeOutcome},
};

struct Rig {
    host: Arc<PeerIdentity>,
    client: Arc<PeerIdentity>,
    service: Arc<PairingService>,
}

async fn rig() -> Rig {
    let host = Arc::new(PeerIdentity::generate().unwrap());
    let service = listen_pairing("127.0.0.1:0".into(), host.clone())
        .await
        .unwrap();
    Rig {
        host,
        client: Arc::new(PeerIdentity::generate().unwrap()),
        service,
    }
}

fn pipe() -> (PipeIo, PipeIo) {
    let (a, b) = tokio::io::duplex(64 * 1024);
    (Box::new(a), Box::new(b))
}

impl Rig {
    async fn pair_over(
        &self,
        device_end: PipeIo,
        code: String,
    ) -> Result<Vec<u8>, PairingNetworkError> {
        pair_with_host_stream(
            device_end,
            self.client.clone(),
            code,
            Some(certificate_fingerprint(&self.host.certificate())),
            OperationCancellation::new(),
        )
        .await
    }

    /// The host's side of a successful pairing: save, then confirm.
    async fn persist(&self) -> Vec<u8> {
        let pending = self.service.next_verified().await.unwrap();
        let certificate = pending.peer_certificate();
        pending.confirm_persisted().await.unwrap();
        certificate
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_pairs_over_a_pipe() {
    let rig = rig().await;
    let (host_end, device_end) = pipe();
    let handle = rig.service.accept_stream(host_end).unwrap();
    let code = rig.service.display_code().unwrap();
    let (paired, saved) = tokio::join!(rig.pair_over(device_end, code), rig.persist());
    assert_eq!(paired.unwrap(), rig.host.certificate());
    assert_eq!(saved, rig.client.certificate());
    assert_eq!(handle.outcome().await, PipeOutcome::Accepted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_code_fails_on_a_pipe_as_on_the_network_and_the_right_one_still_works() {
    let rig = rig().await;
    let code = rig.service.display_code().unwrap();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let (host_end, device_end) = pipe();
    let handle = rig.service.accept_stream(host_end).unwrap();
    assert!(matches!(
        rig.pair_over(device_end, wrong.into()).await,
        Err(PairingNetworkError::Authentication)
    ));
    assert_eq!(handle.outcome().await, PipeOutcome::HandshakeFailed);

    let (host_end, device_end) = pipe();
    let handle = rig.service.accept_stream(host_end).unwrap();
    let (paired, _) = tokio::join!(rig.pair_over(device_end, code), rig.persist());
    assert_eq!(paired.unwrap(), rig.host.certificate());
    assert_eq!(handle.outcome().await, PipeOutcome::Accepted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_that_leaves_after_the_hello_spends_nothing() {
    let rig = rig().await;
    let code = rig.service.display_code().unwrap();
    // More departures than the window has attempts.
    for _ in 0..8 {
        let (host_end, device_end) = pipe();
        let handle = rig.service.accept_stream(host_end).unwrap();
        drop(device_end);
        assert_eq!(handle.outcome().await, PipeOutcome::ClosedIdle);
    }
    let (host_end, device_end) = pipe();
    rig.service.accept_stream(host_end).unwrap();
    let (paired, _) = tokio::join!(rig.pair_over(device_end, code), rig.persist());
    assert!(paired.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_host_s_fingerprint_stops_before_the_code_is_used() {
    let rig = rig().await;
    let code = rig.service.display_code().unwrap();
    let other = PeerIdentity::generate().unwrap();
    let (host_end, device_end) = pipe();
    let handle = rig.service.accept_stream(host_end).unwrap();
    let result = pair_with_host_stream(
        device_end,
        rig.client.clone(),
        code.clone(),
        Some(certificate_fingerprint(&other.certificate())),
        OperationCancellation::new(),
    )
    .await;
    assert!(matches!(result, Err(PairingNetworkError::WrongHost)));
    assert_eq!(handle.outcome().await, PipeOutcome::ClosedIdle);
    // The code was never sent, so it still pairs.
    let (host_end, device_end) = pipe();
    rig.service.accept_stream(host_end).unwrap();
    let (paired, _) = tokio::join!(rig.pair_over(device_end, code), rig.persist());
    assert!(paired.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_window_takes_no_pipe_and_lets_go_of_the_ones_it_held() {
    let rig = rig().await;
    let (host_end, _device_end) = pipe();
    let held = rig.service.accept_stream(host_end).unwrap();
    rig.service.close_and_wait().await.unwrap();
    assert!(rig.service.is_closed());
    assert_eq!(held.outcome().await, PipeOutcome::Refused);
    let (host_end, _device_end) = pipe();
    assert!(matches!(
        rig.service.accept_stream(host_end),
        Err(PairingNetworkError::Closed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_pipes_leave_room_for_pairing_over_the_network() {
    let rig = rig().await;
    // Cabled devices that never pair: as many pipes as the service queues.
    let mut idle = Vec::new();
    let mut refused = 0;
    for _ in 0..12 {
        let (host_end, device_end) = pipe();
        match rig.service.accept_stream(host_end) {
            Ok(handle) => idle.push((handle, device_end)),
            Err(PairingNetworkError::Unavailable) => refused += 1,
            Err(other) => panic!("{other}"),
        }
    }
    assert!(refused > 0, "the queue of pipes is bounded");
    let code = rig.service.display_code().unwrap();
    let network = pair_with_host(
        vec![rig.service.address()],
        rig.client.clone(),
        code,
        None,
        OperationCancellation::new(),
    );
    let (paired, _) = tokio::join!(network, rig.persist());
    assert_eq!(paired.unwrap(), rig.host.certificate());
}
