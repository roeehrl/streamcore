use streamcore_transport::protocol::{
    Hello, MAX_FRAME_BYTES, ProtocolError, read_frame, write_frame,
};
use tokio::io::AsyncWriteExt;

#[test]
fn negotiation_requires_major_and_both_required_sets() {
    let ours = Hello {
        major: streamcore_transport::protocol::PROTOCOL_MAJOR,
        capabilities: vec!["inventory.v1".into(), "terminal.v1".into()],
        required: vec!["inventory.v1".into()],
    };
    let mut peer = Hello {
        major: streamcore_transport::protocol::PROTOCOL_MAJOR,
        capabilities: vec!["inventory.v1".into(), "future.v2".into()],
        required: vec![],
    };
    assert_eq!(ours.negotiate(&peer).unwrap(), vec!["inventory.v1"]);
    peer.major = streamcore_transport::protocol::PROTOCOL_MAJOR + 1;
    assert!(matches!(
        ours.negotiate(&peer),
        Err(ProtocolError::VersionMismatch)
    ));
    peer.major = streamcore_transport::protocol::PROTOCOL_MAJOR;
    peer.required.push("future.v2".into());
    assert!(matches!(
        ours.negotiate(&peer),
        Err(ProtocolError::MissingCapability)
    ));
    peer.required = vec!["unadvertised.v1".into()];
    assert!(matches!(
        ours.negotiate(&peer),
        Err(ProtocolError::InvalidCapabilities)
    ));
    peer.required.clear();
    peer.capabilities.push("inventory.v1".into());
    assert!(matches!(
        ours.negotiate(&peer),
        Err(ProtocolError::InvalidCapabilities)
    ));
}

#[tokio::test]
async fn fragmented_frames_round_trip_without_swift_enum_encoding() {
    let (mut tx, mut rx) = tokio::io::duplex(1);
    let writer = tokio::spawn(async move {
        write_frame(&mut tx, br#"{"kind":"ping","id":"a"}"#)
            .await
            .unwrap();
        write_frame(&mut tx, b"second").await.unwrap();
    });
    assert_eq!(
        read_frame(&mut rx).await.unwrap(),
        br#"{"kind":"ping","id":"a"}"#
    );
    assert_eq!(read_frame(&mut rx).await.unwrap(), b"second");
    writer.await.unwrap();
}

#[tokio::test]
async fn length_is_rejected_before_allocating_or_waiting_for_body() {
    for length in [0, MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
        let (mut tx, mut rx) = tokio::io::duplex(8);
        tx.write_u32(length).await.unwrap();
        assert!(matches!(
            read_frame(&mut rx).await,
            Err(ProtocolError::InvalidFrame)
        ));
    }
}

#[tokio::test]
async fn truncated_body_is_not_a_valid_frame() {
    let (mut tx, mut rx) = tokio::io::duplex(8);
    tx.write_u32(5).await.unwrap();
    tx.write_all(&[1, 2]).await.unwrap();
    drop(tx);
    assert!(matches!(
        read_frame(&mut rx).await,
        Err(ProtocolError::Io(_))
    ));
}
