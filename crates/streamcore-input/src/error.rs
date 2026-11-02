//! The one error type crossing every binding.

/// Why an input-model call failed. Every variant carries an English `detail` for
/// logs (not `message`, which Kotlin exceptions reserve); callers branch on the variant (or, through the JSON ABI, on [`code`]).
///
/// [`code`]: StreamInputError::code
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum StreamInputError {
    /// The text is not JSON, or not the expected JSON shape.
    #[error("invalid JSON: {detail}")]
    InvalidJson { detail: String },
    /// Well-formed JSON whose values break the protocol (unknown enum value,
    /// modifier bit, missing companion field, out-of-range number).
    #[error("invalid message: {detail}")]
    InvalidMessage { detail: String },
    /// A binary payload with the wrong length or an invalid field.
    #[error("invalid binary payload: {detail}")]
    InvalidBinary { detail: String },
    /// A profile that a host must refuse.
    #[error("invalid profile: {detail}")]
    InvalidProfile { detail: String },
    /// JSON ABI: no such method.
    #[error("unknown method: {method}")]
    UnknownMethod { method: String },
    /// JSON ABI: the handle does not name a live object of the expected kind.
    #[error("unknown handle: {handle}")]
    UnknownHandle { handle: u64 },
    /// JSON ABI: the arguments are missing a field or have the wrong type.
    #[error("invalid arguments: {detail}")]
    InvalidArguments { detail: String },
}

impl StreamInputError {
    /// Stable camelCase code used by the JSON ABI and the fixtures.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidJson { .. } => "invalidJson",
            Self::InvalidMessage { .. } => "invalidMessage",
            Self::InvalidBinary { .. } => "invalidBinary",
            Self::InvalidProfile { .. } => "invalidProfile",
            Self::UnknownMethod { .. } => "unknownMethod",
            Self::UnknownHandle { .. } => "unknownHandle",
            Self::InvalidArguments { .. } => "invalidArguments",
        }
    }

    pub(crate) fn json(error: impl std::fmt::Display) -> Self {
        Self::InvalidJson {
            detail: error.to_string(),
        }
    }

    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self::InvalidMessage {
            detail: message.into(),
        }
    }

    pub(crate) fn binary(message: impl Into<String>) -> Self {
        Self::InvalidBinary {
            detail: message.into(),
        }
    }

    pub(crate) fn profile(message: impl Into<String>) -> Self {
        Self::InvalidProfile {
            detail: message.into(),
        }
    }

    pub(crate) fn arguments(message: impl Into<String>) -> Self {
        Self::InvalidArguments {
            detail: message.into(),
        }
    }
}
