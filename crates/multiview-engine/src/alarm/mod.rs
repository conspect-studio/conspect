//! The ITU-T X.733 **alarm engine**: a pure, off-the-hot-path lifecycle over the
//! shared alarm vocabulary in [`multiview_core::alarm`] (ADR-MV001).
//!
//! This module turns a stream of instantaneous probe
//! [`observations`](crate::probe::ProbeObservation) into a coherent alarm state
//! with dwell-up/dwell-down hysteresis, latch and operator acknowledge
//! ([`state`]); rolls per-probe severities up the probe → tile → group → system
//! hierarchy and evaluates Boolean (AND/OR/XOR) virtual alarms ([`rollup`]); and
//! maps a *sustained* alarm to a layout action emitted as a non-blocking engine
//! command ([`penalty_box`]).
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Every type here is a **pure state machine over an injected
//! [`MediaTime`](multiview_core::time::MediaTime)** — exactly the pattern used by
//! the tile failure-ladder ([`multiview_framestore::state`]) and the alert card
//! ([`multiview_overlay::alert`]). There are no timers, no sleeps, no channels and
//! no I/O in this subsystem: the engine *drives* it from its own slow control
//! tick with already-sampled inputs, and the subsystem hands back values
//! (records, severities, commands). It therefore **cannot block the output clock
//! or back-pressure the engine** — a stalled or absent alarm tick simply means
//! the alarm state is not advanced, never that a frame is delayed. The
//! penalty-box action is *returned* as a [`PenaltyAction`] for the engine to
//! apply at a frame boundary; this module never reaches into the engine itself.
pub mod config_seam;
pub mod penalty_box;
pub mod rollup;
pub mod state;

pub use config_seam::{black_config_from_kind, engine_zone, freeze_config_from_kind};
pub use penalty_box::{PenaltyAction, PenaltyBox, PenaltyConfig, PenaltyState};
pub use rollup::{BoolOp, RollupNode, VirtualAlarm};
pub use state::{AlarmHysteresis, AlarmStateMachine, AlarmTransition, Phase};
