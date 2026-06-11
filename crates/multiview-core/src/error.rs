//! Error taxonomy for the workspace.
//!
//! Every fallible operation in `multiview-core` returns [`Result`], whose error
//! arm is the workspace-wide [`enum@Error`] enum. Downstream crates may define their
//! own `thiserror` enums and convert into this taxonomy at their boundary.
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error type spanning the Multiview pipeline stages.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An ingest/source failure.
    #[error("input error: {0}")]
    Input(String),
    /// A decode failure.
    #[error("decode error: {0}")]
    Decode(String),
    /// A compositing failure.
    #[error("compositor error: {0}")]
    Compositor(String),
    /// An encode failure.
    #[error("encode error: {0}")]
    Encode(String),
    /// An output/mux/serve failure.
    #[error("output error: {0}")]
    Output(String),
    /// A configuration or template-validation error.
    #[error("config error: {0}")]
    Config(String),
    /// An audio-pipeline failure: per-input decode/resample/mix/route or EBU
    /// R128 metering. The audio stage ([`multiview-audio`]) is first-class
    /// alongside video decode/encode, so it owns a dedicated arm rather than
    /// folding into [`Error::Config`].
    ///
    /// [`multiview-audio`]: https://docs.rs/multiview-audio
    #[error("audio error: {0}")]
    Audio(String),
    /// A hardware-abstraction / backend-selection failure: capability detection,
    /// per-stage backend negotiation, or admission against an engine budget
    /// (the inputs to the planner in `multiview-hal`). Distinct from
    /// [`Error::Config`] (the operator's request was structurally valid; the
    /// host simply cannot satisfy it as asked).
    #[error("backend error: {0}")]
    Backend(String),
    /// A bounded operation exceeded its deadline (e.g. a GPU readback, a
    /// device-probe, or a graceful-drain wait that is given a finite timeout so
    /// it can never block the data plane forever — safety rule #1). Recoverable:
    /// callers retry, fall back, or hold last-good rather than treating it as a
    /// permanent fault.
    #[error("operation timed out: {0}")]
    Timeout(String),
    /// An operation was cancelled before completing — typically a controlled
    /// shutdown or a reconfiguration that superseded an in-flight request. The
    /// payload names what was cancelled; it is not a failure of the work itself.
    #[error("operation cancelled: cancelled during {0}")]
    Cancelled(String),
}

#[cfg(test)]
mod tests {
    use super::Error;

    /// Every stage arm renders with its documented, distinct prefix. Guards the
    /// `#[error("…")]` Display strings against silent message drift and proves
    /// the new arms format their payload.
    #[test]
    fn display_renders_each_stage_prefix() {
        assert_eq!(
            Error::Input("eof".to_owned()).to_string(),
            "input error: eof"
        );
        assert_eq!(
            Error::Decode("bad nal".to_owned()).to_string(),
            "decode error: bad nal"
        );
        assert_eq!(
            Error::Compositor("no adapter".to_owned()).to_string(),
            "compositor error: no adapter"
        );
        assert_eq!(
            Error::Encode("session".to_owned()).to_string(),
            "encode error: session"
        );
        assert_eq!(
            Error::Output("mux".to_owned()).to_string(),
            "output error: mux"
        );
        assert_eq!(
            Error::Config("bad cadence".to_owned()).to_string(),
            "config error: bad cadence"
        );
        assert_eq!(
            Error::Audio("resample".to_owned()).to_string(),
            "audio error: resample"
        );
        assert_eq!(
            Error::Backend("no cuda device".to_owned()).to_string(),
            "backend error: no cuda device"
        );
        assert_eq!(
            Error::Timeout("readback".to_owned()).to_string(),
            "operation timed out: readback"
        );
        assert_eq!(
            Error::Cancelled("shutdown".to_owned()).to_string(),
            "operation cancelled: cancelled during shutdown"
        );
    }

    /// The new arms are distinct values and carry their payload through `Debug`
    /// (so a `match` on the taxonomy can tell, e.g., a transport timeout from a
    /// backend-selection fault).
    #[test]
    fn new_arms_are_distinct_and_carry_payload() {
        let audio = Error::Audio("x".to_owned());
        let backend = Error::Backend("x".to_owned());
        let timeout = Error::Timeout("x".to_owned());
        let cancelled = Error::Cancelled("x".to_owned());

        let rendered = [
            audio.to_string(),
            backend.to_string(),
            timeout.to_string(),
            cancelled.to_string(),
        ];
        // All four prefixes differ even though the payload is identical.
        for (i, a) in rendered.iter().enumerate() {
            for b in rendered.iter().skip(i + 1) {
                assert_ne!(a, b, "stage prefixes must be distinct");
            }
        }

        assert!(format!("{audio:?}").contains("Audio"));
        assert!(format!("{backend:?}").contains("Backend"));
        assert!(format!("{timeout:?}").contains("Timeout"));
        assert!(format!("{cancelled:?}").contains("Cancelled"));
    }

    /// `From<&str>`/`From<String>` are *not* provided (a stringly-typed blanket
    /// conversion would erase the stage); the canonical authoring path stays the
    /// explicit constructors. This test pins that the `Cancelled` reason is woven
    /// into a fixed human sentence rather than echoed verbatim.
    #[test]
    fn cancelled_wraps_reason_in_sentence() {
        let msg = Error::Cancelled("reconfiguration superseded".to_owned()).to_string();
        assert!(msg.starts_with("operation cancelled: cancelled during "));
        assert!(msg.ends_with("reconfiguration superseded"));
    }
}
