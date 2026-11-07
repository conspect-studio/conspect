//! Source demand status and consumer leases, as the control plane sees them
//! (`docs/architecture/demand-driven-sources.md`).
//!
//! The binary implements [`SourceDemandProvider`] over the engine's demand
//! manager. Every method is bounded and never blocks on a source: a lease is
//! booked and the start happens off-clock on the manager's worker.

use std::sync::Arc;

use serde::Serialize;

/// A source's demand phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceDemandPhase {
    /// No consumer and nothing running. Not a fault.
    Idle,
    /// A consumer needs it and its first media has not arrived yet.
    Starting,
    /// Producing.
    Live,
    /// A consumer needs it but it is not producing. It keeps retrying.
    Failed,
}

/// One consumer of a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceConsumerView {
    /// `layout_cell`, `audio_route`, `subtitle_route`, `preview`, `hls_session`,
    /// `event_automation`, `event_journal` or `always_on`.
    pub kind: String,
    /// The consumer's identity within its kind (a cell, a route target, ...).
    pub label: String,
}

/// One source's demand state.
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
        }
    }
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

    /// Take or extend a short preview lease (for a poller with no close event).
    /// Returns the phase, or `None` when `id` is unknown or the bound on timed
    /// leases is reached.
    fn touch_preview(&self, id: &str) -> Option<SourceDemandPhase>;
}

/// Shared provider handle.
pub type SharedSourceDemand = Arc<dyn SourceDemandProvider>;
