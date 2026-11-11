//! Switcher core types and exact-rational timing (ADR-T015, ADR-0055,
//! ADR-0056).
//!
//! This is the SW-0 foundation the production-switcher backlog builds on:
//!
//! - [`timing`] carries every duration as an integer [`FrameCount`] at the
//!   program's exact rational cadence, converts API milliseconds exactly
//!   (round half up, one-frame floor) and evaluates transition [`progress`] as a
//!   pure `f(tick)`. It performs no floating-point arithmetic at all.
//! - [`descriptor`] carries the transition, keyer and bus-tap descriptors — all
//!   `#[non_exhaustive]` and serde-internally-tagged, never `untagged`.
//! - [`kernel`] is the single, deliberate float bridge to the compositor's
//!   per-tile opacity: float conversion happens only there, at the pixel
//!   boundary.
pub mod descriptor;
pub mod kernel;
pub mod timing;

pub use descriptor::{
    ArmedSet, DveDirection, KeySource, KeyType, KeyerDescriptor, KeyerMode, KeyerTransition,
    PixelRect, SourceRef, TapPoint, TransitionDescriptor, WipePattern,
};
pub use timing::{progress, FrameCount, FrameCountError, Progress};
