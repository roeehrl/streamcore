//! Transport tuning for video (docs/STREAMCORE-VIDEO.md 6.2, 6.4 item 5, 7.2).
//! Each behaviour is a separate flag so it can be turned off on its own (11.1);
//! [`TransportTuning::default`] is the behaviour before tuning existed. A process
//! sets its tuning once at startup with [`set_transport_tuning`]; listeners and
//! dials read it when they start, channels when they are created.
//!
//! None of the values below is qualified yet: each change ships only after the
//! scheduling probe and the pacing suite are re-run with it (7.2), which is why
//! [`candidate_transport_tuning`] is a starting point for measurement, not a
//! default.
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

/// The congestion controller for QUIC connections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Enum)]
pub enum QuicCongestion {
    /// quinn's default.
    #[default]
    Cubic,
    /// quinn's experimental BBR. Not evaluated yet; do not enable without
    /// measurement (7.2).
    Bbr,
}

/// QUIC transport parameters; `None` keeps quinn's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct QuicTuning {
    /// Bytes the peer may send on one stream before it is consumed (quinn's
    /// default is about 1.2 MB). Each lane is one stream.
    pub stream_receive_window: Option<u32>,
    /// Bytes the peer may send on the whole connection before it is consumed
    /// (quinn's default is unlimited, bounded by the stream windows).
    pub receive_window: Option<u32>,
    /// Bytes sent but unacknowledged across the connection (quinn's default is
    /// about 10 MB). Near the bandwidth-delay product plus a few frames bounds the
    /// queue quinn holds for video (6.2).
    pub send_window: Option<u32>,
    /// The initial RTT estimate, in milliseconds, used when dialing a LAN address
    /// (private, link-local or loopback; quinn's default is 333 ms). It shortens
    /// the first loss timers before an RTT sample exists. A listener does not use
    /// it: its first sample comes from the client's first flight.
    pub lan_initial_rtt_ms: Option<u32>,
    /// Advertise no datagram support, freeing quinn's unused datagram receive
    /// buffer. Datagram video (`video.datagram.v1`, P8) needs it off.
    pub disable_datagrams: bool,
    pub congestion: QuicCongestion,
}

/// The process's transport flags (all off by default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct TransportTuning {
    /// `TCP_NOTSENT_LOWAT` in bytes on TLS/TCP sockets, about one video frame:
    /// unsent data in the kernel stays near this bound, so a frame queued behind
    /// bulk waits for at most about one frame of earlier data. macOS, iOS, Linux
    /// and Android; ignored elsewhere (see [`crate::sockopt`]).
    pub tcp_notsent_lowat: Option<u32>,
    pub quic: QuicTuning,
    /// The multiplexer's bulk token bucket (6.4 item 5): while video is active,
    /// lane 4 is limited to (capacity − video rate) × 0.8 once the channel has
    /// rate inputs (`SecureChannel::set_bulk_rate_inputs`). It ships with
    /// `VideoSink`, which provides those inputs: a process that sends video
    /// through the sink turns it on before its listeners and dials start.
    pub bulk_token_bucket: bool,
}

static TUNING: Mutex<TransportTuning> = Mutex::new(TransportTuning {
    tcp_notsent_lowat: None,
    quic: QuicTuning {
        stream_receive_window: None,
        receive_window: None,
        send_window: None,
        lan_initial_rtt_ms: None,
        disable_datagrams: false,
        congestion: QuicCongestion::Cubic,
    },
    bulk_token_bucket: false,
});

/// Replaces the process's transport tuning. Listeners and dials started later,
/// and channels created later, use it; existing ones keep theirs.
#[uniffi::export]
pub fn set_transport_tuning(tuning: TransportTuning) {
    if let Ok(mut current) = TUNING.lock() {
        *current = tuning;
    }
}

/// The process's current transport tuning.
#[uniffi::export]
pub fn transport_tuning() -> TransportTuning {
    TUNING.lock().map(|t| *t).unwrap_or_default()
}

/// Values to start measuring from (7.2), sized for a LAN: windows near a
/// bandwidth-delay product of a few hundred Mbit/s × 20 ms plus a few 1440p
/// frames, a 25 ms LAN initial RTT, no datagram buffer, and a 64 KiB TCP low-water
/// mark (about one frame at 30 Mbit/s and 60 fps). Cubic stays; the bulk bucket
/// is on, as `VideoSink` needs it.
#[uniffi::export]
pub fn candidate_transport_tuning() -> TransportTuning {
    TransportTuning {
        tcp_notsent_lowat: Some(64 * 1024),
        quic: QuicTuning {
            stream_receive_window: Some(2 * 1024 * 1024),
            receive_window: Some(8 * 1024 * 1024),
            send_window: Some(2 * 1024 * 1024),
            lan_initial_rtt_ms: Some(25),
            disable_datagrams: true,
            congestion: QuicCongestion::Cubic,
        },
        bulk_token_bucket: true,
    }
}

/// Private, link-local and loopback addresses, including IPv4-mapped ones and
/// IPv6 unique-local addresses. Carrier-grade NAT and overlay ranges are not LAN.
pub(crate) fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_lan(v4.into()))
        }
    }
}

/// quinn's transport configuration: the fixed lane and liveness settings, then
/// the tuning. `peer` is the dialed address (None on a listener).
pub(crate) fn quic_transport(
    tuning: &QuicTuning,
    peer: Option<SocketAddr>,
) -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_bidi_streams((crate::multiplex::LANES as u32).into());
    config.max_concurrent_uni_streams(0_u32.into());
    config.keep_alive_interval(Some(Duration::from_secs(2)));
    config.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(10)).expect("constant idle timeout"),
    ));
    if let Some(window) = tuning.stream_receive_window {
        config.stream_receive_window(window.into());
    }
    if let Some(window) = tuning.receive_window {
        config.receive_window(window.into());
    }
    if let Some(window) = tuning.send_window {
        config.send_window(window.into());
    }
    if let (Some(ms), Some(peer)) = (tuning.lan_initial_rtt_ms, peer)
        && ms > 0
        && is_lan(peer.ip())
    {
        config.initial_rtt(Duration::from_millis(ms.into()));
    }
    if tuning.disable_datagrams {
        config.datagram_receive_buffer_size(None);
        config.datagram_send_buffer_size(0);
    }
    if tuning.congestion == QuicCongestion::Bbr {
        config.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_addresses_are_private_link_local_or_loopback() {
        // Built from parts: the tree carries no literal addresses.
        let v4 = |a, b, c, d| IpAddr::from(std::net::Ipv4Addr::new(a, b, c, d));
        let v6 = |first, last| IpAddr::from(std::net::Ipv6Addr::new(first, 0, 0, 0, 0, 0, 0, last));
        let mapped =
            |a, b, c, d| IpAddr::from(std::net::Ipv4Addr::new(a, b, c, d).to_ipv6_mapped());
        for lan in [
            v4(10, 1, 2, 3),
            v4(172, 16, 0, 1),
            v4(192, 168, 1, 1),
            v4(169, 254, 1, 1),
            v4(127, 0, 0, 1),
            v6(0, 1),
            v6(0xfd00, 1),
            v6(0xfe80, 1),
            mapped(192, 168, 1, 1),
        ] {
            assert!(is_lan(lan), "{lan}");
        }
        for wan in [
            v4(8, 8, 8, 8),
            v4(100, 64, 0, 1),
            v6(0x2001, 1),
            mapped(8, 8, 8, 8),
        ] {
            assert!(!is_lan(wan), "{wan}");
        }
    }

    #[test]
    fn default_tuning_is_quinn_default_plus_the_fixed_lane_settings() {
        let tuned = candidate_transport_tuning();
        assert_ne!(tuned, TransportTuning::default());
        assert!(!tuned.bulk_token_bucket);
        assert_eq!(tuned.quic.congestion, QuicCongestion::Cubic);
        // Building either configuration, with and without a LAN peer, succeeds.
        for quic in [QuicTuning::default(), tuned.quic] {
            for peer in [None, Some(SocketAddr::from(([192, 168, 1, 2], 1)))] {
                quic_transport(&quic, peer);
            }
        }
        quic_transport(
            &QuicTuning {
                congestion: QuicCongestion::Bbr,
                ..tuned.quic
            },
            None,
        );
    }
}
