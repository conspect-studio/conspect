//! Source demand status and consumer leases, as the control plane sees them
//! (`docs/architecture/demand-driven-sources.md`).
//!
//! The binary implements [`SourceDemandProvider`] over the engine's demand
//! manager. Every method is bounded and never blocks on a source: a lease is
//! booked and the start happens off-clock on the manager's worker.

use std::sync::Arc;

use serde::Serialize;

/// A source's lifecycle state. A separate axis from the tile signal state
/// (`LIVE` / `STALE` / `NO_SIGNAL`) and from tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceDemandPhase {
    /// Not started; costs nothing. Not a fault.
    Idle,
    /// Wanted, and its first media has not arrived yet.
    Starting,
    /// Started and producing.
    Ready,
    /// Wanted but not producing. It retries with backoff.
    Failed,
}

/// Where a `starting` source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceStartingPhase {
    /// Waiting for a start slot.
    Queued,
    /// Opening (connecting, launching).
    Opening,
    /// Started; waiting for its first media.
    AwaitingMedia,
}

/// When a source starts (`sources[].activation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceActivationView {
    /// Started at boot, whether or not anything consumes it (the default).
    AlwaysOn,
    /// Idle until something consumes it.
    OnDemand,
}

impl From<multiview_config::SourceActivation> for SourceActivationView {
    fn from(value: multiview_config::SourceActivation) -> Self {
        match value {
            multiview_config::SourceActivation::OnDemand => Self::OnDemand,
            multiview_config::SourceActivation::AlwaysOn => Self::AlwaysOn,
        }
    }
}

/// One consumer of a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceConsumerView {
    /// `layout_cell`, `audio_route`, `subtitle_route`, `preview`, `hls_session`,
    /// `event_automation`, `always_on` or `manual` (an operator Start).
    pub kind: String,
    /// The consumer's identity within its kind (a cell, a route target, ...).
    pub label: String,
}

/// One source's demand state. Flat booleans keep the wire shape simple for
/// the SPA (one per rail per direction).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceDemandView {
    /// The source id.
    pub id: String,
    /// The phase.
    pub phase: SourceDemandPhase,
    /// Whether consumers want its video.
    pub video_wanted: bool,
    /// Whether consumers want its audio.
    pub audio_wanted: bool,
    /// Whether its video rail is running (a lingering rail runs with no consumer).
    pub video_running: bool,
    /// Whether its audio rail is running.
    pub audio_running: bool,
    /// Who is consuming it.
    pub consumers: Vec<SourceConsumerView>,
    /// Why it is failed. Operator-safe: never a URL or credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// While `starting`: where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_phase: Option<SourceStartingPhase>,
    /// While `starting`: milliseconds left before the start timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_in_ms: Option<u64>,
    /// The source's configured activation. Filled by the API from the
    /// accepted document; providers may leave the default.
    pub activation: SourceActivationView,
}

impl SourceDemandView {
    /// The view of a source nothing consumes.
    #[must_use]
    pub fn idle(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            phase: SourceDemandPhase::Idle,
            video_wanted: false,
            audio_wanted: false,
            video_running: false,
            audio_running: false,
            consumers: Vec::new(),
            detail: None,
            starting_phase: None,
            deadline_in_ms: None,
            activation: SourceActivationView::AlwaysOn,
        }
    }
}

/// Why a manual Stop was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopRefusal {
    /// These consumers still use the source.
    InUse(Vec<SourceConsumerView>),
    /// The run cannot start or stop sources (no demand manager).
    Unsupported,
}

/// A held demand. Dropping it releases the demand.
pub trait SourceDemandLease: Send + Sync {}

/// The demand surface the binary provides.
pub trait SourceDemandProvider: Send + Sync {
    /// The demand state of `id`. An unknown or unconsumed source is idle.
    fn status(&self, id: &str) -> SourceDemandView;

    /// Every source with an active record. Absent sources are idle.
    fn active(&self) -> Vec<SourceDemandView>;

    /// Take a lease on `id`'s video for a preview session labelled `label`.
    /// `None` when `id` is not a configured source.
    fn preview_lease(&self, id: &str, label: &str) -> Option<Box<dyn SourceDemandLease>>;

    /// Take a lease on `id`'s video and audio for an on-demand HLS session
    /// labelled `label` (`feat/rc-on-demand-hls` calls this). `None` when `id`
    /// is not a configured source. The default refuses, for providers without
    /// session support.
    fn session_lease(&self, id: &str, label: &str) -> Option<Box<dyn SourceDemandLease>> {
        let _ = (id, label);
        None
    }

    /// Take or extend a short preview lease (for a poller with no close event).
    /// Returns the phase, or `None` when `id` is unknown or the bound on timed
    /// leases is reached.
    fn touch_preview(&self, id: &str) -> Option<SourceDemandPhase>;

    /// Operator Start: hold `id` ready until Stop. `false` when `id` is not a
    /// configured source or the run cannot start sources. Never blocks.
    fn start_source(&self, id: &str) -> bool {
        let _ = id;
        false
    }

    /// Operator Stop. Refused while other consumers hold the source.
    ///
    /// # Errors
    /// [`StopRefusal`] naming the consumers, or `Unsupported`.
    fn stop_source(&self, id: &str) -> Result<(), StopRefusal> {
        let _ = id;
        Err(StopRefusal::Unsupported)
    }
}

/// Shared provider handle.
pub type SharedSourceDemand = Arc<dyn SourceDemandProvider>;
