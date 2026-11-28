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
//! event.
//!
//! **What the target rectangle scopes.** The rectangle confines the mouse, the pen and touch:
//! every point is clamped into it, and an event with a number that is not finite is refused. It
//! does not scope the keyboard. Keys, shortcuts and text go to whichever window has the keyboard
//! focus, wherever that window is. A host that confines control to one window must give that
//! window the focus before it injects keys, and keep checking that it still has it.
//!
//! **The lease.** Only the holder of the [`InputLease`] may inject. The lease is exclusive and
//! revocable; when it ends, however it ends, every key, button and contact that is held is
//! released and what was still queued is discarded. Ending a lease takes effect at once and
//! never waits for the injection thread; the `_and_wait` calls wait, with a timeout, for the
//! release to be done. A holder whose lease is ended by someone else hears
//! [`Feedback::LeaseRevoked`].
//!
//! Injection runs on the injector's own thread; [`Injector::inject`] queues and returns. The queue
//! is bounded: under pressure pointer moves are merged or dropped, and an event that may not be
//! dropped and does not fit revokes the lease and releases everything, so nothing stays held.
//! [`Capabilities`] (whether a pen and a touch device exist) is read once, when the injector
//! starts.
//!
//! **Desktop state.** [`desktop`] reads what a host reports beside the stream and acts on: the
//! focused window's keyboard layout, the foreground app, the windows a switcher would list,
//! bringing one to the foreground, whether the desktop can take input now, and the Windows
//! version. Those functions inject nothing and need no lease.
//!
//! The rules (key tables, layout mapping, pacing, the pen and touch sessions, the lease,
//! coordinate mapping and what is held) are portable and tested on every platform, behind the
//! [`Backend`] trait. The Win32 calls are in `win`, built on Windows only.

pub mod desktop;
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
mod queue;

#[cfg(windows)]
mod win;

pub use desktop::{AppWindow, ForegroundApp, LayoutFamily, LayoutId, SessionState, WindowId};
pub use engine::{Backend, SendOp};
pub use event::{
    ClickEvent, Feedback, FeedbackSink, InjectError, InputEvent, PanDragEvent, PenEvent,
    RevokeReason, ScrollEvent, TouchEvent, UnavailableKey, ZoomEvent,
};
pub use geometry::{TargetRect, VirtualScreen};
pub use injector::{Capabilities, Injector, InputLease};
pub use layout::{HostKeyState, KeyLayout, KeyStroke, KeyUnavailable};
pub use pen::PenFrame;
pub use queue::{MAX_PENDING_OPERATIONS, MAX_PENDING_TEXT_UNITS};
pub use touch::Contacts;
