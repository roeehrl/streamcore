//! Windows input injection for a stream host: what a client draws, taps and types becomes real
//! Windows input.
//!
//! - **Pen**: a synthetic pointer device (`CreateSyntheticPointerDevice`,
//!   `InjectSyntheticPointerInput`), so pressure, tilt, the eraser and the barrel button reach
//!   painting apps as Windows Ink. Hover moves the pen without touching. Without a pen device the
//!   mouse stands in, so the pointer and clicks still work.
//! - **Touch**: two contacts on a synthetic touch device, so apps see an ordinary pinch, rotate
//!   or pan.
//! - **Mouse**: moves, buttons, the wheel, pinch zoom as wheel notches, and a pan as a middle or
//!   Space drag.
//! - **Keys**: positions by virtual key and scan code; shortcuts resolved by character on the
//!   focused window's keyboard layout (`VkKeyScanExW`), guarded against fallbacks that would
//!   close a window; text typed as layout keys or `KEYEVENTF_UNICODE`, one character at a time.
//!
//! The crate decodes no network messages and knows no product protocol. A host hands
//! [`InputEvent`]s to an [`Injector`], with points normalized to a [`TargetRect`] it supplies per
//! event. Only the holder of the [`InputLease`] may inject: the lease is exclusive and revocable,
//! and revoking it releases every key, button and contact that is held.
//!
//! Injection runs on the injector's own thread; [`Injector::inject`] posts and returns.
//!
//! The rules (key tables, layout mapping, pacing, the pen and touch sessions, the lease,
//! coordinate mapping and what is held) are portable and tested on every platform, behind the
//! [`Backend`] trait. The Win32 calls are in `win`, built on Windows only.

pub mod geometry;
pub mod keys;
pub mod layout;
pub mod lease;
pub mod pacer;
pub mod pan;
pub mod pen;
pub mod touch;

mod engine;
mod event;
mod injector;

#[cfg(windows)]
mod win;

pub use engine::{Backend, SendOp};
pub use event::{
    ClickEvent, Feedback, FeedbackSink, InjectError, InputEvent, PanDragEvent, PenEvent,
    ScrollEvent, TouchEvent, UnavailableKey, ZoomEvent,
};
pub use geometry::{TargetRect, VirtualScreen};
pub use injector::{Capabilities, Injector, InputLease};
pub use layout::{HostKeyState, KeyLayout, KeyStroke, KeyUnavailable};
pub use pen::PenFrame;
pub use touch::Contacts;
