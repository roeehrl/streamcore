//! P5's first piece of the Windows video host (design doc `docs/STREAMCORE-VIDEO.md`, 4.2): a
//! GPU-resident capture -> convert -> hardware-encode path behind the `streamcore-video` traits,
//! and a bench that drives it through a real [`streamcore_video::StreamController`] and reports
//! per-stage timings (9.2) on the QPC clock.
//!
//! With the `bench` feature on Windows, `win` provides:
//!
//! - `win::queue::ExecutorThread`: the stream's `SerialQueue`, one thread (STA, MMCSS "Capture")
//!   that pumps a `DispatcherQueue`, so the WGC frame pool created on it fires `FrameArrived` on
//!   the executor (3.5, 4.2);
//! - `win::capture::WgcCapture`: `CaptureBackend` over Windows Graphics Capture on the encoder's
//!   adapter;
//! - `win::convert::VpConverter`: `Converter`, BGRA -> NV12 at the target size with
//!   `ID3D11VideoProcessor`, fenced with an `ID3D11Fence`;
//! - `win::encoder::MfEncoderFactory`: `EncoderFactory` and `StreamEncoder` over the Media
//!   Foundation hardware H.264 MFT (`MFTEnum2` per adapter LUID, asynchronous events, low-latency
//!   attributes), with Annex-B -> AVCC, avcC and the SPS VUI normaliser on the output thread;
//! - the bench binary `streamcore-video-windows-bench` (`report`, `probe`, `bench`).
//!
//! The portable modules (statistics, the trace recorder, slot ids, the MFT bookkeeping, H.264
//! helpers, the frame-counter barcode, the adapter policy and the report model) build and are
//! tested on every platform.

pub mod barcode;
pub mod cli;
pub mod clock;
pub mod h264;
pub mod mft;
pub mod model;
pub mod policy;
pub mod slots;
pub mod stats;
pub mod trace;

#[cfg(all(windows, feature = "bench"))]
pub mod win;
