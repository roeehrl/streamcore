//! StreamCore's video control plane (docs/STREAMCORE-VIDEO.md, D1–D4).
//!
//! Rust runs the control plane of each stream: lifecycle, validation,
//! admission, latest-frame retention, recovery and sending (the `VideoSink`,
//! straight onto a secure channel's video lane). Native code runs
//! the data plane on GPU surfaces behind the [backend traits](traits). Pixels
//! never cross into Rust: it holds opaque surface leases and metadata, and
//! encoded bytes enter it once, through [`VideoSinkHandle::submit_encoded`] or
//! the per-frame C ABI ([`native`]).
//!
//! Each stream has exactly one serial executor ([`StreamController`] over a
//! [`SerialQueue`]); the threading and reentrancy rules are in [`threading`].

pub mod abr;
pub mod bitstream;
pub mod bounded;
pub mod canvas;
pub mod controller;
pub mod error;
pub(crate) mod executor;
pub mod native;
pub mod pacing;
#[cfg(feature = "ffi-probe")]
pub mod probe;
pub(crate) mod sink;
pub(crate) mod stream;
pub mod traits;
pub mod types;

#[doc = include_str!("../../../docs/VIDEO-THREADING.md")]
pub mod threading {}

pub use bounded::{run_bounded, run_bounded_value};
pub use canvas::{
    CanvasArrangement, CanvasDisplay, CanvasPoint, CanvasSize, NormalizedPoint,
    canvas_input_target, canvas_nearest_member_point, canvas_size, canvas_tiles, canvas_validate,
};
pub use controller::{CanvasParts, StreamController, StreamParts};
pub use error::{MediaError, codes};
pub use executor::{
    Clock, MonotonicClock, NETWORK_THREAD_NAME, StreamPort, VideoSinkHandle, blocking_allowed,
    on_executor,
};
pub use sink::{SendTicket, SinkPolicy, SinkState, SinkTransport, VideoOutput};
/// The design's name for [`StreamEncoder`]: Rust backends may implement either.
pub use traits::StreamEncoder as Encoder;
pub use traits::*;
pub use types::*;

uniffi::setup_scaffolding!();
