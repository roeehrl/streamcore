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

/// How a consumer of a surface waits for the producer's GPU work (3.2). Handle
/// fields name a shareable OS object; `0` means the producer's own in-process
/// object on the stream's device (an `ID3D11Fence` or `MTLSharedEvent` the
/// producing backend owns and looks up through its slot table), so no shared
/// handle is created or closed. A non-zero handle is owned by the producer and
/// stays valid while the surface is leased. When one native backend owns every
/// stage, the fence is informational (threading doc, "Outputs").
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
    /// Maps a source's tags to the tags the converter and encoder output, for an
    /// encoder fed `output` from a `source`-format surface. v1 has no HDR:
    /// BT.2020 primaries and PQ/HLG/linear transfers map to BT.709.
    ///
    /// The matrix and range describe the encoder's YUV, never the source's RGB:
    /// for a YUV output, an RGB source (BGRA, FP16) or an untagged or identity
    /// matrix gives BT.709, and an RGB source is encoded at limited range.
    pub fn map(
        &self,
        source: ColorTags,
        source_format: PixelFormat,
        output: PixelFormat,
    ) -> ColorTags {
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
        if !output.is_yuv() {
            return ColorTags {
                primaries,
                transfer,
                matrix: YCbCrMatrix::Identity,
                full_range: true,
            };
        }
        let rgb_source = !source_format.is_yuv();
        let matrix = match source.matrix {
            YCbCrMatrix::Bt601 if !rgb_source => YCbCrMatrix::Bt601,
            // BT.2020 primaries map to BT.709 in v1, so does their matrix.
            _ => YCbCrMatrix::Bt709,
        };
        ColorTags {
            primaries,
            transfer,
            matrix,
            full_range: source.full_range && !rgb_source,
        }
    }
}

impl PixelFormat {
    /// NV12 and P010 are YUV; BGRA and FP16 are RGB.
    pub fn is_yuv(self) -> bool {
        matches!(self, Self::Nv12 | Self::P010)
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
    /// The GPU adapter to capture on, from [`StreamPlan::adapter_luid`]; the
    /// executor sets it when it opens the capture.
    pub adapter_luid: Option<u64>,
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
    /// For a `Region` target: true when the capture already holds only the
    /// region (macOS `sourceRect`); false when it delivers the whole display
    /// and the converter crops [`ConvertJob::source`] (WGC). A region the
    /// backend cannot crop needs `ConvertPath::Convert`.
    pub region_in_surface: bool,
}

/// How a display's desktop is turned relative to its panel's native scan-out,
/// clockwise (macOS `CGDisplayRotation`; Windows `DISPLAYCONFIG_ROTATION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DisplayRotation {
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

/// A display as the desktop places it. `pixel_width` × `pixel_height`, the
/// origin and `scale` are in desktop coordinates, already rotated: a 2560 × 1440
/// panel turned 90° is 1440 × 2560 here, and `rotation` says how it is turned.
/// A display that mirrors or clones another is no layout at all
/// (`CaptureBackend::observe_layout` returns `None`).
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DisplayLayout {
    pub display_id: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub scale: f64,
    pub origin_x: i32,
    pub origin_y: i32,
    pub rotation: DisplayRotation,
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
    /// The part of the input to convert, in input pixels (even origin and
    /// size); `None` converts the whole input. Set for a `Region` target whose
    /// capture holds the whole display ([`CaptureCaps::region_in_surface`]).
    pub source: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct MemberSurface {
    pub member: u8,
    pub surface: SurfaceId,
}

/// One member's tile in the canvas, in canvas pixels; `member` is the member's
/// index in its layout (also [`MemberSurface::member`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CanvasTile {
    pub member: u8,
    pub rect: Rect,
}

/// What [`crate::Compositor::configure`] prepares: an NV12 (the plan's encoder
/// input) canvas of `width` × `height` with every gap black, tiles drawn from
/// each member's latest surface (scaled to the tile), converted to `color`
/// (gamut and matrix: the members' own tags in, these out).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CanvasPlan {
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<CanvasTile>,
    pub color: ColorTags,
    /// At least permits + 2.
    pub pool_size: u8,
}

/// A compose call's result, returned at once (3.5): the GPU work is recorded and
/// the composite's `fence` says when it is complete; consumers of the surface
/// wait on it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ComposeResult {
    Composed {
        surface: SurfaceInfo,
    },
    /// Every pool surface is leased to Rust; Rust retries when it releases one.
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
    /// Mark long-term reference frames (6.5): set only when the plan's
    /// recovery has [`RecoveryStep::LtrRefresh`], which the planner allows only
    /// for confirmed caps ([`encoder_plan_features`]).
    #[uniffi(default = false)]
    pub ltr: bool,
    /// Two temporal layers, the upper one discardable (6.5): set only when the
    /// plan's extensions have [`FrameExtension::TemporalId`].
    #[uniffi(default = false)]
    pub temporal_layers: bool,
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
            .field("ltr", &self.ltr)
            .field("temporal_layers", &self.temporal_layers)
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
    /// Encode this frame predicted only from an LTR frame the client
    /// acknowledged (6.5); never set with `force_keyframe`. An encoder that
    /// cannot outputs a keyframe or a plain delta (the executor then escalates
    /// to an IDR).
    #[uniffi(default = false)]
    pub ltr_refresh: bool,
    /// A newly acknowledged LTR token (the frame's `LtrInfo::token`), passed
    /// once, on the first submit after the client's acknowledgement arrived.
    #[uniffi(default = None)]
    pub ltr_acknowledged: Option<u32>,
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

/// What [`crate::StreamEncoder::invalidate`] reports (3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum InvalidateOutcome {
    /// Pending output was completed or discarded within the adapter's bound.
    Completed,
    /// The platform call overran the adapter's bound and was left running. The
    /// executor treats the session as hung: it releases the session's permits,
    /// counts `invalidate_timeouts` and posts `ObserverJob::EncoderFailed`.
    TimedOut,
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

/// A frame's LTR state (9.4 `ltr`): `token` is nonzero; `flags` has
/// [`LtrInfo::MARK`] (this frame is stored as LTR `token`) and/or
/// [`LtrInfo::REFRESH`] (this frame is predicted only from LTR `token`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct LtrInfo {
    pub token: u32,
    pub flags: u8,
}

impl LtrInfo {
    pub const MARK: u8 = 0x01;
    pub const REFRESH: u8 = 0x02;

    pub fn marks(&self) -> bool {
        self.token != 0 && self.flags & Self::MARK != 0
    }

    pub fn refreshes(&self) -> bool {
        self.token != 0 && self.flags & Self::REFRESH != 0
    }
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
    /// The adapter capture, convert and encode run on, where the platform has
    /// several (Windows: the DXGI LUID packed as `(HighPart << 32) | LowPart`).
    /// Passed to `CaptureSpec` and `EncoderSpec`. `None`: the platform default.
    pub adapter_luid: Option<u64>,
    /// The source display's refresh interval (ns), supplied by the backend or
    /// consumer from the display mode (macOS: the display link or `NSScreen`;
    /// Windows: the DXGI output mode), never inferred from frame times. Kept
    /// current with [`crate::StreamController::set_refresh_interval`]. `None`:
    /// refresh-aligned admission uses the target interval instead
    /// (`StatsSnapshot::refresh_fallback`).
    #[uniffi(default = None)]
    pub refresh_interval_ns: Option<u64>,
    /// The P2 pacing behaviours, each a rollback switch (11.1).
    pub pacing: PacingPolicy,
}

/// The P2 pacing behaviours (6.2, 6.3, 6.5). Each can be switched off to fall
/// back to P1's admission and recovery (11.1); [`Default`] switches all on.
///
/// Drop before encode (D7) has no switch: since P1 a frame that cannot be
/// submitted only replaces the latest slot (skip cause b), and nothing queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PacingPolicy {
    /// Refresh-aligned admission (6.3): the due time advances by the target
    /// interval from the previous due time, resynchronised after idle, and the
    /// first frame with `H0 >= due - refresh_interval / 2` is admitted; earlier
    /// frames only update the latest slot, which is submitted when due if
    /// nothing newer comes. Frames without an H0 are admitted unpaced.
    /// Off: every frame is admitted at once (P1).
    pub refresh_aligned: bool,
    /// Adaptive encoder permits (6.2): `StreamPlan::permit_target` (the plan's
    /// `max(min_inputs_before_output, 1)`), plus one while the encode time p95
    /// exceeds 0.8 x the frame interval, never above 3. Off: the plan's
    /// `permit_target` as is (P1).
    pub adaptive_permits: bool,
    /// Host-detected loss (6.5): the first recovery fires at once instead of
    /// after `StreamTimings::recovery_delay_ns`; later ones keep at least
    /// `recovery_interval_ns` apart (at most 2 IDRs per second). Off: P1's
    /// delayed recovery.
    pub immediate_recovery: bool,
}

impl Default for PacingPolicy {
    fn default() -> Self {
        Self {
            refresh_aligned: true,
            adaptive_permits: true,
            immediate_recovery: true,
        }
    }
}

impl PacingPolicy {
    /// Every P2 behaviour off: P1's admission and recovery.
    pub fn p1() -> Self {
        Self {
            refresh_aligned: false,
            adaptive_permits: false,
            immediate_recovery: false,
        }
    }
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
    /// `invalidate` calls that reported [`InvalidateOutcome::TimedOut`] or
    /// overran `StreamTimings::invalidate_bound_ns`.
    pub invalidate_timeouts: u64,
    /// `EncoderFactory::create` calls that overran their bound.
    pub create_timeouts: u64,
    pub stale_events: u64,
    pub wire_generation: u64,
    /// Skip cause (a): frames the capture backend dropped unseen
    /// ([`crate::CaptureEvents::skipped`]).
    pub capture_skipped: u64,
    /// Skip cause (d): delta frames the sink shed by the delta age bound, or
    /// the transport refused (not keyframes, not discardable frames).
    pub sender_shed: u64,
    /// Skip cause (e): frames the client flushed, as reported to
    /// [`crate::StreamController::note_client_flush`].
    pub client_flushed: u64,
    /// Not a skip: frames refresh-aligned admission replaced before they were
    /// due (a source faster than the target rate).
    pub pacer_held: u64,
    /// Frames a pacer timer submitted after nothing newer came.
    pub pacer_timer_admits: u64,
    /// The encoder permit target in effect (6.2).
    pub permit_target: u32,
    /// Encode time p95 (submit to output, executor clock) over the recent window, µs.
    pub encode_p95_us: u64,
    /// `VideoSink` (6.4): discardable frames dropped by rule 1.
    #[uniffi(default = 0)]
    pub sink_discardable_shed: u64,
    /// `VideoSink`: queued frames a newer keyframe superseded (rule 3).
    #[uniffi(default = 0)]
    pub sink_superseded: u64,
    /// `VideoSink`: deltas shed by the delta age bound (rule 2; also in
    /// `sender_shed`).
    #[uniffi(default = 0)]
    pub sink_aged_shed: u64,
    /// Admissions the sink held back (6.2: a frame unsent or too many
    /// unconsumed); the frame waited in the latest slot.
    #[uniffi(default = 0)]
    pub sink_held: u64,
    /// The most frames ever unsent at once.
    #[uniffi(default = 0)]
    pub sink_unsent_max: u32,
    /// The consumed-credit RTT (windowed minimum), ms; 0 without a sample.
    #[uniffi(default = 0)]
    pub credit_rtt_ms: u32,
    /// The delta age bound (6.4 rule 2) is in effect: switched on and the plan
    /// confirms LTR or temporal layers.
    #[uniffi(default = false)]
    pub sink_age_bound: bool,
    /// `frameTiming` reports sent (frames.v4 only).
    #[uniffi(default = 0)]
    pub frame_timing_sent: u64,
    /// frames.v4 was negotiated on the sink's channel.
    #[uniffi(default = false)]
    pub frames_v4: bool,
    /// The video rate the sink measured over the last second, bit/s.
    #[uniffi(default = 0)]
    pub video_bps: u64,
    /// The link capacity given to the bulk bucket, bit/s.
    #[uniffi(default = 0)]
    pub bulk_capacity_bps: u64,
    /// `bulk_capacity_bps` is the sink's own estimate (the largest write rate
    /// of a frame into an idle lane over 2 s, at least twice the video rate).
    /// It is **not measured** against the link and not qualified.
    #[uniffi(default = false)]
    pub bulk_capacity_estimated: bool,
    /// `SinkPolicy::bulk_bucket` is on but the channel does not run the bucket
    /// (`TransportTuning::bulk_token_bucket` was off when it was created): bulk
    /// may starve video (6.4 rule 5).
    #[uniffi(default = false)]
    pub bulk_bucket_inactive: bool,
    /// The refresh interval admission aligns to (ns); 0 while refresh-aligned
    /// admission is off.
    pub refresh_interval_ns: u64,
    /// Refresh-aligned admission has no refresh interval and uses the target
    /// interval instead.
    pub refresh_fallback: bool,
    /// Recovery (6.5): LTR refreshes submitted (also in `recoveries`).
    #[uniffi(default = 0)]
    pub ltr_refreshes: u64,
    /// LTR refreshes the encoder answered with neither a refresh nor a
    /// keyframe, or that were lost before they were written; each escalated
    /// the recovery to an IDR.
    #[uniffi(default = 0)]
    pub ltr_refresh_failed: u64,
    /// Client LTR acknowledgements taken (`note_ltr_ack`).
    #[uniffi(default = 0)]
    pub ltr_acks: u64,
    /// Acknowledgements ignored: another generation, a token the current
    /// encoder did not mark, or older than the one held.
    #[uniffi(default = 0)]
    pub ltr_acks_ignored: u64,
    /// Canvas (3.9): composites drawn.
    #[uniffi(default = 0)]
    pub canvas_composed: u64,
    /// Canvas: compose calls that found the composite pool exhausted; each is
    /// retried when a composite surface is released, not on a member's next
    /// image.
    #[uniffi(default = 0)]
    pub canvas_pool_exhausted: u64,
    /// Canvas: complete member images received.
    #[uniffi(default = 0)]
    pub canvas_member_frames: u64,
    /// Canvas: the member age spread of the last composite, the newest member
    /// image's H0 minus the oldest's (H1 where a member has no H0), µs.
    #[uniffi(default = 0)]
    pub canvas_age_spread_us: u64,
    /// Canvas: the largest member age spread so far, µs.
    #[uniffi(default = 0)]
    pub canvas_age_spread_max_us: u64,
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
    /// The source's layout (size, scale, origin) changed; `epoch` counts layout
    /// changes. The consumer re-plans, e.g. with a size update.
    LayoutChanged {
        epoch: u64,
        layout: DisplayLayout,
    },
    /// An encoder session failed and was replaced (or abandoned): an
    /// `invalidate` that timed out, or a recoverable
    /// [`crate::EncoderEvents::session_failed`]. The stream continues.
    EncoderFailed {
        error: MediaError,
    },
    Stats {
        snapshot: StatsSnapshot,
    },
    /// The close completed: nothing more is called on any backend.
    Closed {
        outcome: CloseOutcome,
    },
}

/// A frame's stage times (9.2) as the backends reported them, host-monotonic
/// nanoseconds: H0 and H1 from the capture's [`SurfaceInfo`], H3 and H4 from
/// the encoder's [`EncodeTimings`]. A recovery re-encode keeps the image's H0
/// and H1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Record)]
pub struct FrameTiming {
    /// H0, the source present time, where the platform reports it.
    pub source_present_ns: Option<u64>,
    /// H1, the capture callback.
    pub capture_ns: u64,
    /// H3 and H4.
    pub encode: EncodeTimings,
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
    /// `EncoderFactory::create`'s bound: the executor waits at most this long
    /// for a session (MFT activation or an NVENC open can take over a second
    /// the first time), else the stream reports `stream.encoder`.
    pub create_bound_ns: u64,
}

impl Default for StreamTimings {
    fn default() -> Self {
        Self {
            op_timeout_ns: 10_000_000_000,
            command_timeout_ns: 35_000_000_000,
            recovery_delay_ns: 500_000_000,
            recovery_interval_ns: 500_000_000,
            invalidate_bound_ns: 500_000_000,
            create_bound_ns: 3_000_000_000,
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

/// The recovery steps and frame extensions an encoder's caps allow (3.3, 6.4,
/// 6.5): what a planner puts in [`StreamPlan::recovery`] and
/// [`StreamPlan::extensions`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EncoderPlanFeatures {
    /// Always ends with [`RecoveryStep::Idr`].
    pub recovery: Vec<RecoveryStep>,
    /// Only the layer and LTR extensions (`Discardable`, `TemporalId`, `Ltr`);
    /// the consumer adds the others it sends.
    pub extensions: Vec<FrameExtension>,
}

/// The planner rule for LTR and temporal layers: each only from **confirmed**
/// caps (a probe session accepted the setting and its output carried it), and
/// only when the client takes it (`client_ltr`: it acknowledges LTR tokens
/// after decode and understands the `ltr` extension; `client_temporal_layers`:
/// it understands `discardable` and `temporal_id` and does not treat a missing
/// discardable frame as a chain break). `caps` `None`: no probe, nothing but
/// IDR. Intra refresh is listed when confirmed; no executor step uses it yet.
#[uniffi::export]
pub fn encoder_plan_features(
    caps: Option<EncoderCaps>,
    client_ltr: bool,
    client_temporal_layers: bool,
) -> EncoderPlanFeatures {
    let mut recovery = Vec::new();
    let mut extensions = Vec::new();
    if let Some(caps) = caps {
        if caps.ltr == FeatureSupport::Confirmed && client_ltr {
            recovery.push(RecoveryStep::LtrRefresh);
        }
        if caps.intra_refresh == FeatureSupport::Confirmed {
            recovery.push(RecoveryStep::IntraRefresh);
        }
        if caps.temporal_layers == FeatureSupport::Confirmed && client_temporal_layers {
            extensions.push(FrameExtension::Discardable);
        }
        if recovery.contains(&RecoveryStep::LtrRefresh) {
            extensions.push(FrameExtension::Ltr);
        }
        if caps.temporal_layers == FeatureSupport::Confirmed && client_temporal_layers {
            extensions.push(FrameExtension::TemporalId);
        }
    }
    recovery.push(RecoveryStep::Idr);
    EncoderPlanFeatures {
        recovery,
        extensions,
    }
}
