//! StreamCore's media framing: `frames.v3` headers, the typed binary frames
//! carried on the data lanes, and the video stream continuity policy. No pixel,
//! audio or file buffers cross the native interface.

pub mod binary;
pub mod media;
pub mod stream;

uniffi::setup_scaffolding!();
