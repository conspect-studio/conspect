//! Passthrough (output-routing §12.5, ADR-0110 decision 5): pass a source's
//! coded access units to an output slot without decode or encode, while the
//! output clock still emits exactly one correctly stamped AU per tick
//! (invariant #1).
//!
//! * [`tap`]: the per-source compressed tap (bounded, never blocking).
//! * [`match_rules`]: whether a source fits a slot ([`match_rules::evaluate`]).
//! * [`clock`]: the per-slot passthrough clock (jitter buffer, offset rebase,
//!   stall / parameter-change bridging, cadence-drift correction).
//!
//! Pure Rust and always compiled: classification (strict IDR, reference)
//! arrives as plain [`tap::TapAu`] fields, so this module needs no
//! `multiview-ffmpeg`.

pub mod clock;
pub mod match_rules;
pub mod tap;

pub use clock::{
    BridgeReason, ClockCounters, ClockState, Emission, PassthroughClock, RepeatSource, SlateAu,
    SlateLoop, SlotGate, MAX_PRESENTATION_DELAY,
};
pub use match_rules::{evaluate, profile_rank, MatchMiss, SlotSpec, SourceTraits, StreamMeter};
pub use tap::{CompressedTap, TapAu, TapReader, MAX_AU_BYTES, MAX_TAP_CONSUMERS, TAP_CAPACITY};
