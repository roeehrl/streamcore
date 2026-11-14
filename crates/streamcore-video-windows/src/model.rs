//! The reports the bench prints and writes as JSON. Portable, so reports can be read and compared
//! on any machine.

use crate::h264::sps::Sps;
use crate::stats::Summary;
use crate::trace::Counts;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    /// Index across all adapters, in enumeration order (the bench's `--monitor`).
    pub monitor_index: u32,
    pub device_name: String,
    /// left, top, right, bottom in desktop coordinates.
    pub desktop: [i32; 4],
    pub attached: bool,
    pub bits_per_color: u32,
    pub color_space: String,
    pub refresh_hz: Option<f64>,
    /// Clockwise rotation of the desktop against the panel, degrees.
    pub rotation_deg: u16,
    /// Effective DPI (96 = 100 %).
    pub dpi: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderInfo {
    pub name: String,
    pub codec: String,
    pub hardware: bool,
    pub vendor_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// LUID as 16 hex digits (high part, low part).
    pub luid: String,
    pub dedicated_video_memory_mb: u64,
    pub shared_system_memory_mb: u64,
    pub software: bool,
    pub outputs: Vec<OutputInfo>,
    /// Hardware encoder MFTs `MFTEnum2` returns for this adapter's LUID.
    pub encoders: Vec<EncoderInfo>,
    pub encoder_enum_error: Option<String>,
    /// Whether the video processor offers BGRA in and NV12 out.
    pub bgra_to_nv12: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AdapterReport {
    pub os_build: String,
    pub wgc_supported: bool,
    pub min_update_interval_present: bool,
    pub adapters: Vec<AdapterInfo>,
    /// Software (synchronous) H.264 encoder MFTs.
    pub software_encoders: Vec<EncoderInfo>,
    /// What `policy::choose` picks for each output.
    pub policy: Vec<crate::policy::Choice>,
}

/// One `ICodecAPI` property: what was asked and what the encoder reports back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecSetting {
    pub name: String,
    pub requested: String,
    /// `confirmed`, `differs`, `unsupported` (IsSupported failed), `rejected` (SetValue failed)
    /// or `unreadable` (set, but GetValue failed).
    pub status: String,
    pub readback: Option<String>,
}

/// How an encoder session was created.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EncoderSetup {
    pub name: String,
    pub hardware: bool,
    pub asynchronous: bool,
    pub d3d11_aware: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub low_latency_confirmed: bool,
    pub settings: Vec<CodecSetting>,
    /// `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` on the output stream.
    pub provides_output_samples: bool,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub os_build: String,
    pub adapter: String,
    pub encoder: EncoderSetup,
    /// Inputs accepted before the first output when fed one at a time and waiting for output
    /// (the value that sets the permit target).
    pub min_inputs_before_output: Option<u64>,
    pub permit_target: u8,
    /// Time from the first input to the first output.
    pub first_output_ms: Option<f64>,
    /// `ProcessInput` -> `ProcessOutput`, one frame in flight.
    pub encode_ms: Option<Summary>,
    pub sps: Option<Sps>,
    pub errors: Vec<String>,
    /// The 1 ms timer period was requested, and what was applied.
    pub timer: TimerReport,
}

/// The 1 ms timer period (`--timer-period`): requested, `timeBeginPeriod(1)` accepted, and the
/// process opted out of timer-resolution power throttling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerReport {
    pub requested: bool,
    pub period_1ms: bool,
    pub throttling_opt_out: bool,
    /// The throttling state was unreadable, so the opt-out was skipped.
    pub throttling_unreadable: bool,
}

/// One bench run (`bench --mode synthetic|desktop`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    pub os_build: String,
    pub mode: String,
    pub adapter: String,
    pub monitor: u32,
    pub capture_size: [u32; 2],
    pub encode_size: [u32; 2],
    pub fps_target: u32,
    pub refresh_hz: Option<f64>,
    pub seconds: f64,
    pub warmup_seconds: f64,
    pub bitrate: u64,
    pub permit_target: u8,
    pub pool_buffers: u8,
    pub min_update_interval_set: bool,
    pub timer: TimerReport,
    pub encoder: EncoderSetup,
    /// `min_inputs_before_output` the probe found, and what the bench saw in the stream.
    pub min_inputs_before_output_probe: Option<u64>,
    pub inputs_before_first_output: Option<u64>,
    pub max_frames_in_encoder: Option<u64>,
    /// Frames per second over the measured window.
    pub fps_captured: f64,
    pub fps_submitted: f64,
    pub fps_encoded: f64,
    /// Frames pushed to a real `VideoSink` per second; `None` with the bench's discard sink,
    /// which measures nothing.
    pub fps_sent: Option<f64>,
    /// Recorder counters over the whole run.
    pub counts: Counts,
    /// The stream's own counters (`StreamController::stats`) at the end.
    pub stream: BTreeMap<String, u64>,
    pub stages: BTreeMap<String, Summary>,
    /// R1: frames whose H4 -> H5 gap exceeded one frame interval; `None` with the discard sink.
    pub r1_late_sends: Option<u64>,
    pub synthetic: Option<SyntheticResult>,
    pub encoded_mbps: f64,
    pub keyframes: u64,
    pub sps: Option<Sps>,
    pub vui_normalized: bool,
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyntheticResult {
    /// How the source changed: a new counter (and colour) on every composition, or every N ms.
    pub pace: String,
    /// Counters drawn inside the measured window (with a margin at both ends).
    pub counters_drawn: u64,
    /// Distinct counters the encoder saw.
    pub counters_encoded: u64,
    /// Drawn counters no submitted frame showed first (skipped before or at capture, or replaced
    /// in the latest slot).
    pub counters_skipped: u64,
    /// Submitted frames that showed a counter already submitted.
    pub repeated: u64,
    /// Submitted frames whose barcode could not be read.
    pub undecoded: u64,
}
