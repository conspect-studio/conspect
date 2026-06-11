//! Content-aware fault **probes** over sampled, last-good frames (ADR-MV001).
//!
//! These probes inspect *decoded essence* — a luma view of an NV12 frame, or a
//! frame's observed [`FrameMeta`](multiview_core::frame::FrameMeta) — and report
//! whether a fault **condition** is present *right now*. They are deliberately
//! **stateless, allocation-free analysers**: each [`detect`](BlackProbe::detect)
//! call is a pure function of its inputs and returns a [`ProbeObservation`]
//! (condition present / absent, plus a measured value for diagnostics).
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Probes **never block and never pace anything**. They consume frames the
//! engine has already *sampled* from the per-tile last-good store (the same
//! wait-free slot the compositor reads), so running a probe cannot stall the
//! output clock or back-pressure an input. A probe that is starved of frames
//! simply isn't called; it holds no resource an input or a client can hold. The
//! dwell/hysteresis lifecycle that turns a stream of these instantaneous
//! observations into a *raised/cleared alarm* lives in
//! [`crate::alarm::state`] and is likewise a pure state machine over an injected
//! [`MediaTime`](multiview_core::time::MediaTime) — no real time, no sleeps.
//!
//! All thresholds, detection zones and dwells are per-probe and individually
//! configurable so any probe can be made cheaper or disabled to protect the
//! output-clock invariant (ADR-MV001 consequences).
mod black;
mod format;
mod freeze;
mod luma;

pub use black::{BlackConfig, BlackProbe};
pub use format::{ExpectedFormat, FormatAxis, FormatMismatch, FormatProbe};
pub use freeze::{FreezeConfig, FreezeProbe};
pub use luma::{DetectionZone, LumaView, LumaViewError};

use multiview_core::alarm::AlarmKind;

/// The instantaneous result of evaluating a probe against one sampled frame.
///
/// This is a *point-in-time* reading, **not** an alarm: it says only whether the
/// fault condition is present in this frame. The dwell-up/dwell-down hysteresis
/// that decides when the condition has persisted long enough to *raise* (or has
/// recovered long enough to *clear*) an X.733 alarm is applied separately by
/// [`crate::alarm::state::AlarmStateMachine`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeObservation {
    /// Which fault class this observation belongs to.
    pub kind: AlarmKind,
    /// Whether the fault condition is present in the sampled frame.
    pub condition_present: bool,
    /// The measured statistic that drove the decision, for diagnostics and UI
    /// (e.g. mean luma for [`BlackProbe`], the changed-sample fraction for
    /// [`FreezeProbe`]). Units are probe-specific and documented per probe.
    pub measured: f64,
}

impl ProbeObservation {
    /// Construct an observation.
    #[must_use]
    pub const fn new(kind: AlarmKind, condition_present: bool, measured: f64) -> Self {
        Self {
            kind,
            condition_present,
            measured,
        }
    }
}
