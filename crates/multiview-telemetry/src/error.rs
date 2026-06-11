//! Error taxonomy for the telemetry crate.
//!
//! Telemetry is best-effort and must never back-pressure or crash the engine
//! (invariant #10). These errors surface only at *configuration* time — e.g. a
//! malformed tracing-filter directive supplied by an operator — never on a
//! data-plane path.
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, TelemetryError>;

/// Errors raised while configuring telemetry.
///
/// `#[non_exhaustive]` so new variants can be added without a breaking change;
/// downstream `match` statements must carry a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TelemetryError {
    /// A `tracing` env-filter directive failed to parse.
    #[error("invalid tracing filter directive: {0}")]
    Filter(String),

    /// A syslog message could not be delivered to its collector.
    ///
    /// Only constructed when the off-by-default `syslog` transport feature is
    /// enabled; delivery is best-effort and this error never reaches the engine
    /// data plane (invariant #10).
    #[cfg(feature = "syslog")]
    #[error("syslog transport error: {0}")]
    Transport(String),
}
