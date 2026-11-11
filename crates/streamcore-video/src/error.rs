//! Errors and wire codes (3.7). The codes are listed in `protocol/STREAM-CODES.md`.

/// A backend or validation failure. Reactions (3.7): `Unsupported` at plan time
/// picks the next encoder or codec; `DeviceLost` rebuilds the encoder with a new
/// generation and IDR (more than 3 in 10 s fails the stream); a recoverable
/// encoder failure runs recovery; `OutOfResources` drops the frame and retries
/// when a permit or surface is released, never on a fixed timer; `Timeout` enters
/// an `*Unknown` phase with the capture retained.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum MediaError {
    #[error("invalid settings")]
    InvalidSettings,
    #[error("unsupported")]
    Unsupported,
    #[error("permission denied")]
    PermissionDenied,
    #[error("source gone")]
    SourceGone,
    #[error("device lost")]
    DeviceLost,
    #[error("out of resources")]
    OutOfResources,
    #[error("encoder failed (recoverable: {recoverable})")]
    EncoderFailed { recoverable: bool },
    #[error("timed out")]
    Timeout,
    #[error("internal error")]
    Internal,
}

/// Wire codes, ported unchanged from the reference host; the strings are the
/// contract.
pub mod codes {
    // Commands.
    pub const CONFIG: &str = "stream.config";
    pub const LIMIT: &str = "stream.limit";
    pub const BUSY: &str = "stream.busy";
    pub const CANCELLED: &str = "stream.cancelled";
    pub const TIMEOUT: &str = "stream.timeout";
    pub const COMMAND: &str = "stream.command";
    pub const MISSING: &str = "stream.missing";
    // Mode.
    pub const VIEW_ONLY: &str = "stream.viewOnly";
    pub const VIEW_ONLY_UNAVAILABLE: &str = "stream.viewOnlyUnavailable";
    pub const OWN_WINDOW: &str = "stream.ownWindow";
    // Sources.
    pub const TARGET: &str = "stream.target";
    pub const DIMENSIONS: &str = "stream.dimensions";
    pub const CONTENT: &str = "stream.content";
    pub const VIEWPORT: &str = "stream.viewport";
    pub const FIT: &str = "stream.fit";
    pub const SCALE: &str = "stream.scale";
    pub const RESTORE: &str = "stream.restore";
    // Capture and encode.
    pub const START: &str = "stream.start";
    pub const STOP: &str = "stream.stop";
    pub const UPDATE: &str = "stream.update";
    pub const CAPTURE: &str = "stream.capture";
    pub const OUTPUT: &str = "stream.output";
    pub const ENCODER: &str = "stream.encoder";
    pub const ENCODE: &str = "stream.encode";
    pub const SEQUENCE: &str = "stream.sequence";
    // Canvas.
    pub const FIRST_FRAME: &str = "stream.firstFrame";
    pub const MEMBER_UNAVAILABLE: &str = "stream.memberUnavailable";
    pub const LAYOUT_CHANGED: &str = "stream.layoutChanged";
    // Sessions (8.7).
    pub const SESSION_STALE: &str = "session.stale";
    /// Permission codes are `permission.*` (docs/INPUT-MODEL.md).
    pub const PERMISSION_PREFIX: &str = "permission.";

    /// Every `stream.*` and `session.*` code above.
    pub const ALL: &[&str] = &[
        CONFIG,
        LIMIT,
        BUSY,
        CANCELLED,
        TIMEOUT,
        COMMAND,
        MISSING,
        VIEW_ONLY,
        VIEW_ONLY_UNAVAILABLE,
        OWN_WINDOW,
        TARGET,
        DIMENSIONS,
        CONTENT,
        VIEWPORT,
        FIT,
        SCALE,
        RESTORE,
        START,
        STOP,
        UPDATE,
        CAPTURE,
        OUTPUT,
        ENCODER,
        ENCODE,
        SEQUENCE,
        FIRST_FRAME,
        MEMBER_UNAVAILABLE,
        LAYOUT_CHANGED,
        SESSION_STALE,
    ];
}

impl MediaError {
    /// The wire code for this error when it ends the stage `stage` (one of
    /// [`codes`]). Invalid settings, a gone source and a timeout keep their own
    /// code; everything else is reported as the stage that failed.
    pub fn wire_code(&self, stage: &'static str) -> &'static str {
        match self {
            Self::InvalidSettings => codes::CONFIG,
            Self::SourceGone => codes::CONTENT,
            Self::Timeout => codes::TIMEOUT,
            _ => stage,
        }
    }
}
