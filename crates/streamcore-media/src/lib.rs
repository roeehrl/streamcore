//! StreamCore's media framing: `frames.v3` headers and the `frames.v4` extension
//! block, the typed binary frames carried on the data lanes (including the lane-0
//! video kinds), and the video stream continuity policy. No pixel, audio or file
//! buffers cross the native interface.

pub mod binary;
pub mod media;
pub mod stream;
pub mod video_control;

uniffi::setup_scaffolding!();
