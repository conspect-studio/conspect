//! Error taxonomy for the web engine's resource-accounting core.
//!
//! Every failure here is an *accounting* failure, never a data-plane one: the
//! sampler runs off the hot path and a scope that cannot be read is reported as
//! unknown rather than propagated into the engine (invariants #1 and #10).

use std::io;

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised by the resource-scope, sampler and job-queue core.
///
/// `#[non_exhaustive]` so new variants can be added without a breaking change;
/// downstream `match` statements must carry a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A backing read failed (a `/proc` or cgroup file that exists but could not
    /// be read, or a fixture root that could not be walked).
    #[error("io error reading {path}: {source}")]
    Io {
        /// The path that failed, as it was requested (root-relative).
        path: String,
        /// The underlying I/O failure.
        source: io::Error,
    },

    /// A file that must exist for the requested reading was absent.
    #[error("required file {path} is absent")]
    Missing {
        /// The path that was absent, as it was requested (root-relative).
        path: String,
    },

    /// A file was present but a field inside it did not parse.
    #[error("malformed {path}: {detail}")]
    Parse {
        /// The path the malformed content came from.
        path: String,
        /// What was wrong with it.
        detail: String,
    },

    /// A job state transition that the lifecycle does not allow.
    #[error("illegal job transition for {job}: {from} -> {to}")]
    IllegalTransition {
        /// The job whose transition was refused.
        job: String,
        /// The state the job was in.
        from: &'static str,
        /// The state that was requested.
        to: &'static str,
    },

    /// A trigger arrived for a job id that is not tracked, or a terminal
    /// transition named a job that is not running.
    #[error("unknown job {job}")]
    UnknownJob {
        /// The job id that is not tracked.
        job: String,
    },

    /// The bounded queue (or its concurrency window) cannot take another job.
    #[error("job queue is full: {limit} job(s) already queued or running")]
    QueueFull {
        /// The bound that was hit (`capacity` or `concurrency`).
        limit: usize,
    },

    /// A number that must be a percentage did not fit `u8`.
    #[error("value {value} is not a valid percentage")]
    NotAPercent {
        /// The out-of-range value.
        value: u32,
    },
}

impl Error {
    /// Build an [`Error::Io`] for `path` from `source`.
    ///
    /// Public because implementing [`FsRoot`](crate::fsroot::FsRoot) outside
    /// this crate — a test fixture root is the intended case — means
    /// constructing this crate's error type.
    #[must_use]
    pub fn io(path: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Build an [`Error::Missing`] for `path`.
    #[must_use]
    pub fn missing(path: impl Into<String>) -> Self {
        Self::Missing { path: path.into() }
    }

    /// Build an [`Error::Parse`] for `path` from a static-or-owned description.
    #[must_use]
    pub fn parse(path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Parse {
            path: path.into(),
            detail: detail.into(),
        }
    }
}
