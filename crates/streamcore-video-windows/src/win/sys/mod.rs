//! The only module tree in the crate that may use `unsafe` (see the crate's `Cargo.toml`).
//!
//! windows-rs exposes every Win32 COM method as an `unsafe fn`. Each submodule wraps one API
//! family behind safe types and functions; every `unsafe` block states why the call is sound.
//! The backends, the executor thread and the bench outside `sys` are safe Rust.
#![allow(unsafe_code)]

pub mod com;
pub mod d3d;
pub mod mf;
pub mod vp;
pub mod window;
