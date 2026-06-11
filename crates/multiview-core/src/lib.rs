//! # multiview-core
//!
//! Shared types and traits for the **Multiview** live video multiview engine.
//! This crate is pure-Rust (no FFI) and is depended on by every other crate.
//! See `docs/architecture/conventions.md` for the canonical model and invariants.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod alarm;
pub mod color;
pub mod error;
pub mod frame;
pub mod layout;
pub mod pixel;
pub mod stream;
pub mod tally;
pub mod time;
pub mod traits;
pub mod wallclock;

pub use error::{Error, Result};
