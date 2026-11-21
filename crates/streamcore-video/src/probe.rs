//! The binding smoke surface (3.8): an async foreign trait that Swift and Kotlin
//! implement, awaited from Rust, and an async Rust export awaited from them. If
//! this round trip fails on a platform, `ByteDuplex` becomes a sync callback
//! pair. Built only with the `ffi-probe` feature.

use std::sync::Arc;

#[uniffi::export(rust, foreign)]
#[async_trait::async_trait]
pub trait AsyncProbe: Send + Sync {
    /// Return `value + 1`, after suspending at least once.
    async fn bump(&self, value: u64) -> u64;
}

/// Awaits `probe.bump` `rounds` times starting from `start`.
#[uniffi::export]
pub async fn probe_round_trip(probe: Arc<dyn AsyncProbe>, start: u64, rounds: u32) -> u64 {
    let mut value = start;
    for _ in 0..rounds {
        value = probe.bump(value).await;
    }
    value
}

/// A listener on a loopback port for the bindings' own tests (a host and a
/// client stream in one process): binary lanes, as `connect_secure` dials.
/// Products listen through their own crate, with their own lane policy.
#[uniffi::export]
pub async fn probe_listen(
    identity: Arc<streamcore_transport::identity::PeerIdentity>,
    trusted_peer: Vec<u8>,
    options: streamcore_transport::channel::SessionOptions,
) -> Result<
    Arc<streamcore_transport::channel::SecureListener>,
    streamcore_transport::channel::ChannelError,
> {
    streamcore_transport::channel::listen_secure(
        "127.0.0.1:0".to_owned(),
        identity,
        vec![trusted_peer],
        options,
    )
    .await
}
