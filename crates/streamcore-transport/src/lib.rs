//! StreamCore's secure transport: pinned mutual-TLS identities, OPAQUE pairing,
//! the versioned hello, QUIC and TLS/TCP sessions with path racing, and the
//! bounded multiplexed lanes a [`channel::SecureChannel`] carries. It knows no
//! application messages: a product routes its own control frames to lanes with a
//! [`multiplex::LanePolicy`].

pub mod channel;
pub mod clock;
pub mod identity;
pub mod multiplex;
pub mod pairing;
pub mod pairing_transport;
pub mod protocol;
mod runtime;
mod sockopt;
pub mod transport;
pub mod tuning;
pub use runtime::OperationCancellation;

uniffi::setup_scaffolding!();
