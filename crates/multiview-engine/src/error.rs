//! The per-crate error taxonomy for `multiview-engine`.
//!
//! Engine-level failures are configuration/startup errors (a bad cadence, an
//! invalid layout, a malformed control-loop budget) — the **hot path itself
//! never returns an error to a caller**: per invariants #1 and #2 the output
//! clock and the drive loop hold the last-good frame (or a `NoSignal` card) and
//! keep ticking rather than propagating a failure outward. These variants
//! therefore describe the *setup* seams, where a `Result` is the right contract.
use multiview_core::time::Rational;
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised when configuring or starting the engine.
///
/// `#[non_exhaustive]`: downstream `match` statements must include a wildcard
/// arm so new variants can be added without a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The output cadence is not a usable positive rational (invariant #1/#3:
    /// the clock must have an exact, non-degenerate `num/den` frame rate).
    #[error("invalid output cadence {num}/{den}: must be a positive, non-degenerate rational")]
    InvalidCadence {
        /// The offending numerator.
        num: i64,
        /// The offending denominator.
        den: i64,
    },

    /// The active layout failed structural validation (delegated to
    /// [`multiview_core::layout::Layout::validate`]).
    #[error("invalid layout: {0}")]
    InvalidLayout(String),

    /// The admission/degradation control loop was given a malformed budget or
    /// hysteresis configuration (delegated to `multiview-hal`).
    #[error("invalid control-loop configuration: {0}")]
    InvalidControlLoop(String),

    /// A canvas geometry was rejected by the compositor (e.g. odd dimensions).
    #[error("compositor rejected canvas geometry: {0}")]
    Canvas(String),

    /// A live cell re-point ([`crate::CompositorDrive::rebind_cell`]) targeted an
    /// unknown cell id or an unknown/undeclared source. The binding is held
    /// unchanged — never a panic, never a silent mis-route (RT-6 / ADR-0034).
    #[error("cannot re-point cell: {0}")]
    Rebind(String),

    /// A per-stream route ([`crate::route::RouteApplier`]) could not be applied:
    /// its [`StreamRef`](multiview_config::routing::StreamRef) did not resolve in
    /// the input inventory, the destination (cell / bus channel / subtitle layer)
    /// is unknown, or the underlying re-point primitive failed. The live
    /// crosspoint is held unchanged — never a panic, never a silent mis-route
    /// (RT-11 / ADR-0034).
    #[error("cannot apply route: {0}")]
    Route(String),

    /// A [`MultiviewProgram`](crate::MultiviewProgram) was constructed from a
    /// [`ProgramSpec`](multiview_config::ProgramSpec) whose
    /// [`ProgramKind`](crate::ProgramKind) is not `Multiview` (ADR-0030 MP-0).
    /// The guarded-passthrough and transcode kinds run through their own program
    /// types (MP-3/MP-4); building a multiview program from the wrong kind is a
    /// caller assembly error surfaced as a typed error rather than a panic.
    #[error("multiview program built from non-multiview spec: kind {0:?}")]
    WrongProgramKind(&'static str),

    /// A [`ProgramSet`](crate::ProgramSet) was asked to
    /// [`start`](crate::ProgramSet::start) a program whose id is **already**
    /// running (program ids are unique within a set, ADR-0030 §2.2 / §6). The
    /// existing program is left untouched; the duplicate is rejected rather than
    /// silently replacing or shadowing a running program.
    #[error("program id already running: {0}")]
    DuplicateProgram(String),

    /// A permanent HA cluster-transport fault while *submitting* a heartbeat or
    /// replication message for publication (a malformed-encoding or a hard
    /// socket fault — never a transient drop, which is silent and best-effort).
    /// Gated to the off-by-default `cluster` feature; the default build's error
    /// taxonomy is unchanged.
    #[cfg(feature = "cluster")]
    #[error("HA cluster transport: {0}")]
    Cluster(String),
}

impl Error {
    /// Construct an [`Error::InvalidCadence`] from a [`Rational`].
    #[must_use]
    pub(crate) const fn invalid_cadence(cadence: Rational) -> Self {
        Self::InvalidCadence {
            num: cadence.num,
            den: cadence.den,
        }
    }

    /// Construct an [`Error::DuplicateProgram`] for a program id already running.
    #[must_use]
    pub(crate) fn duplicate_program(id: &str) -> Self {
        Self::DuplicateProgram(id.to_owned())
    }
}
