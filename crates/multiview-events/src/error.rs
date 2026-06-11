//! Error taxonomy for the realtime-events crate.
//!
//! Every fallible operation here returns [`Result`], whose error arm is the
//! crate-local [`enum@Error`] enum (`thiserror`). Downstream crates (the control
//! plane) convert these into their own boundary errors.
use thiserror::Error;

use crate::envelope::SchemaVersion;

/// Convenient result alias used throughout the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors produced while sequencing, validating, or routing realtime frames.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Error {
    /// A per-connection sequence cursor wrapped past [`u64::MAX`]. A live
    /// connection cannot reach this in any realistic deployment; reaching it
    /// signals a logic error rather than an expected runtime condition.
    #[error("sequence counter overflowed u64::MAX")]
    SeqOverflow,

    /// A received frame advertised an envelope schema major the receiver does
    /// not speak. Per ADR-RT002 a client rejects an unknown major.
    #[error("unsupported envelope schema version: got {got:?}, supported {supported:?}")]
    UnsupportedSchemaVersion {
        /// The major the frame declared.
        got: SchemaVersion,
        /// The majors this receiver can decode.
        supported: Vec<SchemaVersion>,
    },

    /// A delta arrived with a sequence at or below the snapshot/last-seen
    /// sequence for its topic — i.e. it would violate per-topic monotonic
    /// ordering (snapshot ⊕ ordered deltas = truth, ADR-RT003).
    #[error("non-monotonic frame: seq {got} does not advance past {last} on topic {topic}")]
    NonMonotonic {
        /// The offending frame's sequence.
        got: u64,
        /// The last sequence already accepted on the topic.
        last: u64,
        /// The topic the ordering violation occurred on (wire form).
        topic: String,
    },
}
