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
    /// `restricted` when it names an object the caller may not read.
    pub label: String,
    /// The authorizable object `label` names (an automation pin's owning
    /// source). Not on the wire: used to redact `label` per principal (review
    /// DMD-04).
    #[serde(skip)]
    pub object_id: Option<String>,
}

/// The label shown in place of a holder naming an object out of scope.
pub const RESTRICTED_LABEL: &str = "restricted";

impl SourceConsumerView {
    /// A consumer of `kind` labelled `label`, naming no other object.
    #[must_use]
    pub fn new(kind: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            label: label.into(),
            object_id: None,
        }
    }

    /// The same consumer, naming `object` (when `Some`).
    #[must_use]
    pub fn naming(mut self, object: Option<&str>) -> Self {
        self.object_id = object.map(str::to_owned);
        self
    }

    /// Hide the label unless `visible` permits the object it names.
    pub fn redact_unless(&mut self, visible: impl Fn(&str) -> bool) {
        if self
            .object_id
            .as_deref()
            .is_some_and(|object| !visible(object))
        {
            RESTRICTED_LABEL.clone_into(&mut self.label);
            self.object_id = None;
        }
    }
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

/// Sized RAII wrapper for one sustained M/E PVW lease. The backing store owns
/// only this one latest handle; replacing it releases the old source after the
/// new source has already been acquired (make-before-break).
pub struct SwitcherPreviewLease {
    /// Keep the on-demand producer wanted until the M/E arms another source.
    #[allow(dead_code)]
    lease: Box<dyn SourceDemandLease>,
}

impl SwitcherPreviewLease {
    /// Wrap an acquired preview lease for the shared latest-state cell.
    #[must_use]
    pub fn new(lease: Box<dyn SourceDemandLease>) -> Self {
        Self { lease }
    }
}

/// A pending resource reservation, not an operation-status record.
struct PendingPreviewLease {
    lease: SwitcherPreviewLease,
    target: String,
    accepted_tick: u64,
    accepted_at: std::time::Instant,
}

/// Resource reconciliation counts. These are NOT operation outcomes: a released
/// reservation may have applied and subsequently been superseded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SwitcherLeaseSweep {
    /// Reservations retained as the current preview demand.
    pub adopted: usize,
    /// Reservations no longer needed by the current preview.
    pub dropped: usize,
    /// Reservations with no authoritative picture yet.
    pub retained: usize,
    /// Reservations cancelled at teardown.
    pub cancelled: usize,
}

#[derive(Default)]
struct PreviewLeaseState {
    closed: bool,
    control: Option<multiview_engine::switcher_control::SwitcherControl>,
    generation: u64,
    current: Option<SwitcherPreviewLease>,
    pending: std::collections::BTreeMap<String, PendingPreviewLease>,
}

/// Bounded control-only resource owner. One lock serializes admission, resolution,
/// reconciliation and permanent detach. No engine clock path takes this lock.
#[derive(Default)]
pub struct SwitcherPreviewLeases {
    state: std::sync::Mutex<PreviewLeaseState>,
}

impl SwitcherPreviewLeases {
    /// Maximum outstanding reservations.
    pub const MAX_PENDING: usize = 64;
    /// Committed frames (not tick indices) sufficient to drain the 64-slot FIFO.
    pub const DRAIN_TICKS_BOUND: u64 = 6;
    /// Minimum age before resource reconciliation.
    pub const RECONCILE_FLOOR: std::time::Duration = std::time::Duration::from_millis(250);

    /// Seed a new owner's initial demand. A detached owner cannot reopen.
    pub fn seed(&self, lease: SwitcherPreviewLease) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.closed {
            state.current = Some(lease);
        }
    }

    /// Reserve and enqueue under the same control-only lock so the drain bound
    /// starts after enqueue, not before a possibly descheduled HTTP task.
    /// # Errors
    /// Returns the lease if closed, full, duplicate, or enqueue fails.
    pub fn admit(
        &self,
        id: String,
        target: String,
        lease: SwitcherPreviewLease,
        control: &multiview_engine::switcher_control::SwitcherControl,
        enqueue: impl FnOnce() -> Result<u64, ()>,
    ) -> Result<(), SwitcherPreviewLease> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed
            || state.pending.len() >= Self::MAX_PENDING
            || state.pending.contains_key(&id)
        {
            return Err(lease);
        }
        let Ok(accepted_tick) = enqueue() else {
            return Err(lease);
        };
        state.control = Some(control.clone());
        state.pending.insert(
            id,
            PendingPreviewLease {
                lease,
                target,
                accepted_tick,
                accepted_at: std::time::Instant::now(),
            },
        );
        Ok(())
    }

    /// Resolve an exact receipt, but choose current demand from the snapshot,
    /// not receipt order (engine batches emit receipts in reverse order).
    pub fn resolve(&self, id: &str, applied: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.pending.remove(id) {
            if applied
                && state
                    .control
                    .as_ref()
                    .and_then(|c| c.snapshot())
                    .is_some_and(|s| s.preview.as_str() == entry.target)
            {
                state.current = Some(entry.lease);
            }
        }
    }

    /// Reconcile resources only; never infer applied/rejected operation status.
    /// `applied_tick` is the committed-frame COUNT read before the snapshot.
    #[must_use]
    pub fn sweep(
        &self,
        engine_preview: Option<&str>,
        applied_tick: u64,
        now: std::time::Instant,
        max: usize,
    ) -> SwitcherLeaseSweep {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut report = SwitcherLeaseSweep::default();
        let candidates: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, entry)| {
                now.saturating_duration_since(entry.accepted_at) >= Self::RECONCILE_FLOOR
                    && applied_tick.saturating_sub(entry.accepted_tick) >= Self::DRAIN_TICKS_BOUND
            })
            .take(max)
            .map(|(id, _)| id.clone())
            .collect();
        for id in candidates {
            let Some(preview) = engine_preview else {
                report.retained += 1;
                continue;
            };
            if let Some(entry) = state.pending.remove(&id) {
                if entry.target == preview {
                    state.current = Some(entry.lease);
                    report.adopted += 1;
                } else {
                    report.dropped += 1;
                }
            }
        }
        report
    }

    /// Permanently close this run's owner; late requests and receipts cannot
    /// resurrect demand. A new boot must construct a new owner.
    pub fn detach(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.generation = state.generation.saturating_add(1);
        let cancelled = state.pending.len();
        state.pending.clear();
        state.current = None;
        cancelled
    }

    /// Pending resource reservations.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .len()
    }
    /// Whether a current demand is held.
    #[must_use]
    pub fn has_current(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .is_some()
    }
    /// Teardown generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }
}

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
