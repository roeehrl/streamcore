use streamcore_transport::{
    identity::PeerIdentity,
    pairing::{PairingClient, PairingError, PairingWindow},
};

#[test]
fn one_time_pin_authenticates_both_certificates_and_persistence_ack() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let window = PairingWindow::new(host.certificate()).unwrap();
    let pair = PairingClient::new(
        window.display_code().unwrap(),
        host.certificate(),
        client.certificate(),
    )
    .unwrap();
    let challenge = window.begin(client.certificate(), pair.request()).unwrap();
    let finalization = pair.finish(challenge.response).unwrap();
    let verified = window
        .finish(challenge.attempt, finalization.clone())
        .unwrap();
    assert_eq!(verified.peer_certificate, client.certificate());
    assert_eq!(
        pair.confirm_persisted(verified.persisted_confirmation.clone())
            .unwrap(),
        host.certificate()
    );
    assert!(window.finish(challenge.attempt, finalization).is_err());
    assert!(window.begin(client.certificate(), pair.request()).is_err());
    assert!(
        pair.confirm_persisted(verified.persisted_confirmation)
            .is_err()
    );
    assert!(window.display_code().is_err());
}

#[test]
fn wrong_code_does_not_authenticate_and_attempt_budget_survives_disconnect() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let window = PairingWindow::new(host.certificate()).unwrap();
    let actual = window.display_code().unwrap();
    let wrong = if actual == "000000" {
        "111111"
    } else {
        "000000"
    };
    let pair = PairingClient::new(wrong.into(), host.certificate(), client.certificate()).unwrap();
    let challenge = window.begin(client.certificate(), pair.request()).unwrap();
    assert!(matches!(
        pair.finish(challenge.response),
        Err(PairingError::Authentication)
    ));
    // A malicious client may simply disappear after each response. Beginning
    // another attempt still consumes the same host-wide window budget.
    for _ in 1..5 {
        let pair =
            PairingClient::new(actual.clone(), host.certificate(), client.certificate()).unwrap();
        window.begin(client.certificate(), pair.request()).unwrap();
    }
    assert!(matches!(
        window.begin(client.certificate(), vec![]),
        Err(PairingError::Locked)
    ));
}

#[test]
fn substituted_server_or_client_certificate_fails_pake_authentication() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let attacker = PeerIdentity::generate().unwrap();
    for substitute_host in [true, false] {
        let window = PairingWindow::new(host.certificate()).unwrap();
        let claimed_host = if substitute_host {
            attacker.certificate()
        } else {
            host.certificate()
        };
        let presented_client = if substitute_host {
            client.certificate()
        } else {
            attacker.certificate()
        };
        let pair = PairingClient::new(
            window.display_code().unwrap(),
            claimed_host,
            client.certificate(),
        )
        .unwrap();
        let challenge = window.begin(presented_client, pair.request()).unwrap();
        assert!(matches!(
            pair.finish(challenge.response),
            Err(PairingError::Authentication)
        ));
    }
}

#[test]
fn forged_persistence_ack_cannot_release_a_trusted_pin() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let window = PairingWindow::new(host.certificate()).unwrap();
    let pair = PairingClient::new(
        window.display_code().unwrap(),
        host.certificate(),
        client.certificate(),
    )
    .unwrap();
    let challenge = window.begin(client.certificate(), pair.request()).unwrap();
    let finalization = pair.finish(challenge.response).unwrap();
    let verified = window.finish(challenge.attempt, finalization).unwrap();
    assert!(pair.confirm_persisted(vec![0; 32]).is_err());
    assert!(
        pair.confirm_persisted(verified.persisted_confirmation)
            .is_err()
    );
}

#[test]
fn cancel_burns_outstanding_pairing_and_malformed_requests_consume_budget() {
    let host = PeerIdentity::generate().unwrap();
    let client = PeerIdentity::generate().unwrap();
    let window = PairingWindow::new(host.certificate()).unwrap();
    let pair = PairingClient::new(
        window.display_code().unwrap(),
        host.certificate(),
        client.certificate(),
    )
    .unwrap();
    let challenge = window.begin(client.certificate(), pair.request()).unwrap();
    let finalization = pair.finish(challenge.response).unwrap();
    window.cancel().unwrap();
    assert!(matches!(
        window.finish(challenge.attempt, finalization),
        Err(PairingError::Finished)
    ));
    let window = PairingWindow::new(host.certificate()).unwrap();
    for _ in 0..5 {
        assert!(window.begin(vec![], vec![]).is_err());
    }
    assert!(matches!(
        window.begin(client.certificate(), pair.request()),
        Err(PairingError::Locked)
    ));
}
