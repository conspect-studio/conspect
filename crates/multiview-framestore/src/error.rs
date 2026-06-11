//! Error taxonomy for `multiview-framestore`.
//!
//! The crate is on the data plane (invariant #2), so its fallible surface is
//! deliberately tiny: the only thing that can go wrong when *configuring* a tile
//! store is supplying nonsensical staleness thresholds. Reads and writes
//! themselves are infallible — a reader always gets either a frame or an
//! explicit `NoSignal` indicator, and a writer always succeeds (newest wins).
use thiserror::Error;

/// Convenient result alias for the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors produced when constructing or configuring a tile store.
///
/// `#[non_exhaustive]` so new variants can be added without a breaking change;
/// downstream `match` statements must carry a wildcard arm.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The supplied staleness thresholds are not strictly increasing
    /// (`hold < stale < nosignal`), so the failure ladder would be ambiguous.
    ///
    /// Carries the three offending nanosecond values in ladder order.
    #[error(
        "tile thresholds must be strictly increasing (hold < stale < nosignal); \
         got hold={hold_ns}ns, stale={stale_ns}ns, nosignal={nosignal_ns}ns"
    )]
    NonMonotonicThresholds {
        /// The `hold` threshold, in nanoseconds.
        hold_ns: i64,
        /// The `stale` threshold, in nanoseconds.
        stale_ns: i64,
        /// The `nosignal` threshold, in nanoseconds.
        nosignal_ns: i64,
    },

    /// A threshold was zero or negative; thresholds must be positive durations.
    #[error("tile threshold must be a positive duration; got {0}ns")]
    NonPositiveThreshold(i64),
}
