//! Platform-independent stream input logic shared by Pad's apps (PadMac, PadPad,
//! PadDroid, PadWin) and, later, MyCNC's StreamCore.
//!
//! The crate owns the input decisions that must be identical on every platform:
//! the Pad 1.3 JSON wire types and their strict/tolerant decoding, the app profile
//! format with its merge rules and built-in presets, modifier latching and typing
//! routes, the redo/guard rules and two-finger gesture math. Native code keeps file
//! IO, event injection, keyboard-layout lookups and UI. See `docs/INPUT-MODEL.md`.
//!
//! Every object is thread-safe (one short internal lock) and never calls back into
//! native code. Swift and Kotlin use the UniFFI bindings; C# uses the JSON C ABI in
//! `streamcore-input-ffi`, which dispatches through [`json_api::JsonApi`].

uniffi::setup_scaffolding!();

pub mod actions;
pub mod constants;
pub mod error;
pub mod gestures;
pub mod json_api;
pub mod keys;
pub mod latch;
pub mod modifiers;
pub mod pencil;
pub mod presets;
pub mod profile;
pub mod shortcuts;
pub mod store;
pub mod time;
pub mod typing;
pub mod vocabulary;
pub mod wire;

mod serde_util;

pub use actions::{ActionSlot, PencilGesture, PencilResponse, PencilSystemPreference};
pub use error::StreamInputError;
pub use gestures::{
    FallbackAccumulator, FallbackCounts, FallbackKind, FallbackOutput, GestureSeries,
    TwoFingerOutput, TwoFingerRouter, TwoFingerTracker,
};
pub use json_api::JsonApi;
pub use keys::{HostKey, NamedKey, UsKey};
pub use latch::{LatchState, ReleaseAll};
pub use modifiers::HostOs;
pub use pencil::{PencilPhase, PencilSample, PointerKind, StrokeButton};
pub use presets::{PresetFile, PresetLibrary};
pub use profile::{
    Fallbacks, HostShortcut, HostShortcuts, Profile, ProfileAction, ProfileDefaults,
    ProfileGestures, ProfileGroup, ProfileIssue, ProfilePen, QuickKey, Shortcut,
};
pub use shortcuts::{GuardDecision, ShortcutResolution};
pub use store::{DeleteOutcome, LoadReport, ProfileStore, UpsertOutcome, UpsertResult};
pub use typing::{HostCapabilities, LatchRoute, OutgoingInput, RoutedInput};
pub use vocabulary::{
    ActionType, GestureDelivery, KeyColor, PadAction, PanDelivery, TwoFingerMode,
};
pub use wire::*;

/// Result type used throughout the crate.
pub type Result<T, E = StreamInputError> = std::result::Result<T, E>;

/// Version of the input model contract implemented by this build. Bumped when a
/// fixture's expected output changes meaning.
pub const INPUT_MODEL_VERSION: u32 = 1;

/// The input model contract version (see [`INPUT_MODEL_VERSION`]).
#[uniffi::export]
pub fn input_model_version() -> u32 {
    INPUT_MODEL_VERSION
}
