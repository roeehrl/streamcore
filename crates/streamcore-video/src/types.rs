//! Records and enums that cross the backend traits (3.2–3.4). Trait records carry
//! metadata only: pixels stay native, and encoded bytes enter Rust once, through
//! [`crate::VideoSinkHandle::submit_encoded`].

use crate::error::MediaError;
use crate::executor::VideoSinkHandle;
use std::sync::Arc;

/// Index + generation into a native slot table; 0 is invalid.
pub type SurfaceId = u64;
/// A capture opened by a [`crate::CaptureBackend`].
pub type CaptureId = u64;
/// Identifies one `wake_after` request; stale tokens are ignored.
pub type TimerToken = u64;

/// Every operation carries a token; results for a stale token are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Record)]
pub struct OpToken {
    pub id: u64,
    /// The stream epoch: incremented on every start.
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SurfaceKind {
    IoSurface,
    D3d11Texture {
        adapter_luid: u64,
    },
    DmaBuf {
        fourcc: u32,
        modifier: u64,
        planes: u8,
    },
    CpuMapped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GpuFence {
    None,
    MtlSharedEvent { handle: u64, value: u64 },
    D3d11Fence { shared_handle: u64, value: u64 },
    SyncFile { fd: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PixelFormat {
    Nv12,
    Bgra,
    P010,
    Fp16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ColorPrimaries {
    Unspecified,
    Bt709,
    DisplayP3,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TransferFunction {
    Unspecified,
    Bt709,
    Srgb,
    Linear,
    Pq,
    Hlg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum YCbCrMatrix {
    Unspecified,
    Bt601,
    Bt709,
    Bt2020,
    Identity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ColorTags {
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: YCbCrMatrix,
    pub full_range: bool,
}

impl ColorTags {
    pub const BT709: Self = Self {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        matrix: YCbCrMatrix::Bt709,
        full_range: false,
    };
}

/// The plan's per-client colour mapping (3.3): source tags in, output tags out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ColorMapping {
    /// The client declared `p3` and can decode it.
    pub allow_p3: bool,
    /// The client has no sRGB transfer value (Android): send sRGB as BT.709.
    pub srgb_transfer_as_bt709: bool,
}

impl ColorMapping {
    /// Maps source tags to the tags the encoder and converter output. v1 has no
    /// HDR: BT.2020 primaries and PQ/HLG/linear transfers map to BT.709.
    pub fn map(&self, source: ColorTags) -> ColorTags {
        let primaries = match source.primaries {
            ColorPrimaries::DisplayP3 if self.allow_p3 => ColorPrimaries::DisplayP3,
            ColorPrimaries::Unspecified => ColorPrimaries::Unspecified,
            _ => ColorPrimaries::Bt709,
        };
        let transfer = match source.transfer {
            TransferFunction::Srgb if !self.srgb_transfer_as_bt709 => TransferFunction::Srgb,
            TransferFunction::Unspecified => TransferFunction::Unspecified,
            _ => TransferFunction::Bt709,
        };
        let matrix = match source.matrix {
            YCbCrMatrix::Bt2020 => YCbCrMatrix::Bt709,
            other => other,
        };
        ColorTags {
            primaries,
            transfer,
            matrix,
            full_range: source.full_range,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A rectangle in 0–1 coordinates of its source.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct NormalizedRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Rotation {
    None,
    Rotate90,
    Rotate180,
    Rotate270,
}

/// A native surface as Rust sees it: metadata only. `*_ns` values are
/// host-monotonic nanoseconds, converted natively.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SurfaceInfo {
    pub id: SurfaceId,
    pub kind: SurfaceKind,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub color: ColorTags,
    /// H1, the capture callback.
    pub capture_ns: u64,
    /// H0, the source present time, where the platform reports it.
    pub source_present_ns: Option<u64>,
    pub dirty: Option<Vec<Rect>>,
    pub rotation: Rotation,
    pub fence: GpuFence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StreamMode {
    View,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum OnSourceGone {
    Fail,
    FallBackToMain,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum CaptureTarget {
    Display {
        display_id: u32,
    },
    Window {
        window_id: u64,
    },
    App {
        app_id: String,
    },
    Region {
        display_id: u32,
        rect: NormalizedRect,
    },
    Canvas {
        display_ids: Vec<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CaptureTargetKind {
    Display,
    Window,
    App,
    Region,
    Canvas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CursorMode {
    Hidden,
    Composited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ColorRequest {
    /// Keep the source's tags.
    Source,
    Bt709,
    DisplayP3,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct CaptureSpec {
    pub target: CaptureTarget,
    pub mode: StreamMode,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub min_interval_ns: u64,
    pub format: PixelFormat,
    pub color: ColorRequest,
    pub cursor: CursorMode,
    pub queue_depth: u8,
}

/// A live capture change: frame interval, region origin, cursor. Absent fields
/// are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct CaptureUpdate {
    pub min_interval_ns: Option<u64>,
    pub region: Option<NormalizedRect>,
    pub cursor: Option<CursorMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CaptureCaps {
    pub target_kinds: Vec<CaptureTargetKind>,
    pub window_fit: bool,
    pub live_region_move: bool,
    pub cursor_modes: Vec<CursorMode>,
    pub surface_kinds: Vec<SurfaceKind>,
    pub pool_depth: u8,
    pub virtual_displays: bool,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DisplayLayout {
    pub display_id: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub scale: f64,
    pub origin_x: i32,
    pub origin_y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WindowSource {
    pub window_id: u64,
    pub app_id: String,
    pub title: String,
    pub on_screen: bool,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SourceCatalog {
    pub displays: Vec<DisplayLayout>,
    pub windows: Vec<WindowSource>,
}

/// The capture callback's frame status. Only `Complete` carries a new image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FrameStatus {
    Complete,
    Idle,
    Blank,
    Suspended,
    Started,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SourceGone {
    Closed,
    Unavailable,
    PermissionRevoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ConvertJob {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub color: ColorTags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct MemberSurface {
    pub member: u8,
    pub surface: SurfaceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CanvasTile {
    pub member: u8,
    pub rect: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CanvasPlan {
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<CanvasTile>,
    pub color: ColorTags,
    /// At least permits + 2.
    pub pool_size: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ComposeResult {
    Composed { surface: SurfaceInfo },
    PoolExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Codec {
    H264,
    Hevc,
    /// Reserved; not offered in v1.
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FeatureSupport {
    Confirmed,
    Requested,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EncoderCaps {
    pub codec: Codec,
    pub name: String,
    pub hardware: bool,
    pub ltr: FeatureSupport,
    pub temporal_layers: FeatureSupport,
    pub intra_refresh: FeatureSupport,
    pub low_latency_rate_control: FeatureSupport,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub inputs: Vec<PixelFormat>,
    pub force_idr: bool,
    pub dynamic_rate: bool,
    pub min_inputs_before_output: u8,
    pub vui_checked: bool,
}

/// What [`crate::EncoderFactory::create`] builds. `sink` is where the encoder's
/// output thread hands encoded bytes to Rust (once per payload).
#[derive(Clone, uniffi::Record)]
pub struct EncoderSpec {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub input_format: PixelFormat,
    pub color: ColorTags,
    pub adapter: Option<u64>,
    pub sink: Arc<VideoSinkHandle>,
}

impl std::fmt::Debug for EncoderSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncoderSpec")
            .field("codec", &self.codec)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("bitrate_bps", &self.bitrate_bps)
            .field("input_format", &self.input_format)
            .field("color", &self.color)
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

/// What the encoder actually created: the announced codec is this one (2.5).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EncoderInfo {
    pub codec: Codec,
    pub name: String,
    pub hardware: bool,
    pub low_latency: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SubmitOpts {
    /// Tag every output and drop with this; it maps to the platform's per-frame
    /// reference (`sourceFrameRefCon`, `presentationTimeUs`).
    pub submit_seq: u64,
    /// Strictly increasing submission time, re-stamped for recovery frames.
    pub pts_ns: u64,
    /// H0 or H1 of the image, kept on recovery re-encodes.
    pub capture_ns: u64,
    pub force_keyframe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SubmitResult {
    /// Accepted; consumes `submit_seq`.
    Submitted,
    /// Not accepted now; consumes no sequence. The permit is released.
    Busy,
    /// Not accepted; consumes no sequence. The permit is released.
    Failed { error: MediaError },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct RateUpdate {
    pub bitrate_bps: u64,
    pub fps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BitstreamForm {
    Avcc4,
    AnnexB,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct LtrInfo {
    pub token: u32,
    pub flags: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Record)]
pub struct EncodeTimings {
    /// H3, encoder input accepted.
    pub input_ns: Option<u64>,
    /// H4, access unit available.
    pub output_ns: Option<u64>,
}

/// An encoder output: metadata and payload tokens. The bytes were handed over
/// with [`crate::VideoSinkHandle::submit_encoded`] before this was posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct EncodedFrame {
    pub submit_seq: u64,
    pub pts_ns: u64,
    pub keyframe: bool,
    /// The codec configuration (avcC/hvcC) submitted with this frame, if any.
    pub config_token: Option<u64>,
    pub payload_token: u64,
    pub form: BitstreamForm,
    pub discardable: bool,
    pub temporal_id: Option<u8>,
    pub ltr: Option<LtrInfo>,
    pub timings: EncodeTimings,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum DropReason {
    /// The encoder consumed the frame without producing output (a loss).
    NoOutput,
    /// The encoder reported an error for this frame.
    Error { error: MediaError },
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct StreamConfig {
    pub max_long_edge_px: Option<u32>,
    pub max_fps: u32,
    pub quality: f32,
    pub bitrate_ceiling_bps: Option<u64>,
    pub codec: Codec,
    pub shows_cursor: bool,
    pub on_source_gone: OnSourceGone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ConvertPath {
    /// The capture format is the encoder's input; no convert call.
    Identity,
    /// Scale and/or colour-convert through [`crate::Converter`].
    Convert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FrameExtension {
    Timing,
    CaptureUs,
    Discardable,
    Ltr,
    TemporalId,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryStep {
    LtrRefresh,
    IntraRefresh,
    Idr,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct StreamPlan {
    pub mode: StreamMode,
    pub codec: Codec,
    pub encode_width: u32,
    pub encode_height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub convert: ConvertPath,
    /// The format the encoder is fed (the converter's output, or the capture's).
    pub encoder_input: PixelFormat,
    pub encoder: String,
    pub color_out: ColorTags,
    pub color_mapping: ColorMapping,
    pub extensions: Vec<FrameExtension>,
    /// `max(min_inputs_before_output, 1)` up to 3.
    pub permit_target: u8,
    pub recovery: Vec<RecoveryStep>,
    pub on_source_gone: OnSourceGone,
    pub medium_cap_bps: Option<u64>,
}

/// A command's result, posted to the observer queue.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum OpOutcome {
    Completed,
    /// Refused or failed; `code` is a wire code (3.7).
    Rejected {
        code: String,
    },
    /// A native operation did not answer in time; the capture is retained.
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CloseOutcome {
    Closed,
    /// The backend's stop failed or did not answer; native state is unknown.
    StopUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StreamPhase {
    Idle,
    Starting,
    StartUnknown,
    Running,
    Updating,
    UpdateUnknown,
    Stopping,
    StopUnknown,
    StopFailed,
    Closing,
    Closed,
}

/// Counters for consumer diagnostics; the full per-stage set lands with P2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Record)]
pub struct StatsSnapshot {
    pub frames_received: u64,
    /// Skip cause (b): the latest slot replaced before it was submitted.
    pub frames_replaced: u64,
    pub submitted: u64,
    pub encoded: u64,
    /// Skip cause (c).
    pub encoder_drops: u64,
    pub packets_sent: u64,
    pub packets_refused: u64,
    pub recoveries: u64,
    pub encoders_created: u64,
    pub invalidate_timeouts: u64,
    pub stale_events: u64,
    pub wire_generation: u64,
}

/// Jobs for the consumer, posted to its [`crate::ObserverQueue`] and never run on
/// an executor.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ObserverJob {
    /// The capture runs; `info.codec` is the codec actually created.
    Started {
        plan: StreamPlan,
        info: EncoderInfo,
    },
    Failed {
        code: String,
    },
    OpResult {
        op: OpToken,
        outcome: OpOutcome,
    },
    SourceGone {
        reason: SourceGone,
    },
    LayoutChanged {
        epoch: u64,
    },
    Stats {
        snapshot: StatsSnapshot,
    },
    /// The close completed: nothing more is called on any backend.
    Closed {
        outcome: CloseOutcome,
    },
}

/// Metadata for one packet handed to [`crate::VideoOut::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct VideoOutMeta {
    pub header: streamcore_media::media::VideoMetadata,
    /// The sender may drop this under pressure (a delta, never config or keyframe).
    pub droppable: bool,
    pub keyframe: bool,
    pub config: bool,
}

/// Time bounds of the executor. Defaults are the reference host's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct StreamTimings {
    /// A native op without `op_done` becomes `*Unknown` (2.5: 10 s).
    pub op_timeout_ns: u64,
    /// A command without a result is answered `OutcomeUnknown` (2.5: 35 s).
    pub command_timeout_ns: u64,
    /// Delay before a host-detected loss is recovered (2.3: 0.5 s).
    pub recovery_delay_ns: u64,
    /// Minimum spacing of recovery keyframes (2.3: at most 2 per second).
    pub recovery_interval_ns: u64,
    /// `Encoder::invalidate`'s bound (3.5, Q9: 500 ms).
    pub invalidate_bound_ns: u64,
}

impl Default for StreamTimings {
    fn default() -> Self {
        Self {
            op_timeout_ns: 10_000_000_000,
            command_timeout_ns: 35_000_000_000,
            recovery_delay_ns: 500_000_000,
            recovery_interval_ns: 500_000_000,
            invalidate_bound_ns: 500_000_000,
        }
    }
}

/// A command's update: rate, capture parameters, encode size. A size change
/// re-creates the encoder (new generation, config + IDR).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct StreamUpdate {
    pub rate: Option<RateUpdate>,
    pub capture: CaptureUpdate,
    pub encode_width: Option<u32>,
    pub encode_height: Option<u32>,
}
