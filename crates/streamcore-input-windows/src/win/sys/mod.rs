//! The only module tree in the crate that may use `unsafe` (see the crate's `Cargo.toml`).
//!
//! windows-rs exposes every Win32 function as an `unsafe fn`. Each submodule wraps one API family
//! behind safe functions and types; every `unsafe` block states why the call is sound.
#![allow(unsafe_code)]

pub mod input;
pub mod layout;
pub mod pointer;
