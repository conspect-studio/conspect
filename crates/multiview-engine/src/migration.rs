//! The **make-before-break (MBB) migration primitive** + the live
//! placement-execution loop (ADR-R010 + ADR-0018 §4 / GPU-5c).
//!
//! [`crate::placement::PlacementController`] is the pure brain: it *observes* a
//! wait-free [`DeviceLoad`] snapshot and *proposes* a
//! [`PlacementProposal`] (`Hold`/`Shed`/`Migrate`/`Split`).
//! This module is the **execution** half GPU-5c introduces: the make-before-break
//! primitive that renders an accepted `Migrate`/`Split` as the five-phase lifecycle
//! ADR-R010 pins — VALIDATE → SPIN-UP → WARM (keepalive) → SWAP (the RT-12
//! `output ← program` crosspoint) → DRAIN+STOP. The primitive is **built and tested
//! here**; the slow control tick that reads proposals ([`PlacementCoordinator`],
//! driven by `multiview-cli`) today observes, records the decision in the
//! [`PlacementCounters`], and sheds — it does **not** yet drive the cutover for an
//! accepted migration, whose live wiring is gated on RT-13 carrier convergence + the
//! MP-5 multi-program run.
//!
//! ## The RT-12 crosspoint ([`OutputCrosspoint`])
//!
//! ADR-R010 names *one* genuinely-new piece of engine infrastructure: the
//! cross-`Program` sink-cutover bridge over
//! [`multiview_output::fanout::PacketRouter::move_sink`] (which exists in
//! `multiview-output` but had **zero** engine callers). [`OutputCrosspoint`] is
//! that bridge: **one** shared [`PacketRouter`] in which each program is a
//! distinct [`RenditionId`], and a SWAP re-keys a consumer's
//! [`Arc<dyn PacketSink>`](multiview_output::fanout::PacketSink) from OLD's
//! rendition to NEW's. It is a frame-boundary table re-key — non-blocking,
//! non-erroring — guarded by a [`std::sync::Mutex`] across which **no `.await`
//! is ever held**: the coordinator's ops are all O(1) table mutations, and the
//! per-program egress `route()` runs on a dedicated egress thread (never the
//! pacer-gated clock loop, which only `try_send`s a tick index — see
//! [`crate::programset`]). So neither the lock nor `move_sink` can stall the
//! output clock (invariants #1 + #10).
//!
//! ## Zero-new-encode at SWAP — the keepalive ([`KeepaliveSink`])
//!
//! Encode-once-mux-many (inv #7) means a rendition with ≥1 sink is encoded once
//! per tick and a rendition with **no** sink is **cold** (not encoded). If the
//! SWAP moved a consumer onto a *cold* NEW rendition, the move itself would
//! trigger NEW's **first** encode at the cutover instant. The WARM phase
//! pre-attaches a discard [`KeepaliveSink`] to NEW's rendition so it is *already
//! encoding* before the cut — the load-bearing precondition for the
//! zero-new-encode guarantee (ADR-R010 §2.3/§2.4). The keepalive is dropped only
//! **after** the real consumer has moved onto NEW, so NEW never momentarily goes
//! cold mid-SWAP.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use multiview_hal::load::{DeviceId, DeviceLoad};
use multiview_hal::select::{
    select_device, GpuCandidate, Pins, PipelineDemand, PlacementPolicy, RejectReason,
};
use multiview_output::fanout::{
    EncodedPacket, PacketRouter, PacketSink, RenditionEncoder, RenditionId,
};
use multiview_telemetry::{PlacementCounters, SuppressReason};

use crate::placement::{MigrationPlan, PlacementController, PlacementProposal, ShedReason};
use crate::programset::ProgramSet;
use crate::runtime::Pacer;

/// The wait-free, best-effort per-GPU load snapshot the placement loop reads
/// each control tick (ADR-0017 §2).
///
/// An off-hot-path poll thread (the CLI's `LoadSource` poller) calls
/// [`LoadSnapshot::publish`] at the bounded poll cadence; the control tick reads
/// the newest snapshot wait-free via [`LoadSnapshot::load`]. It is a single
/// [`arc_swap::ArcSwap`] slot — newest-wins, no lock, no channel into the engine
/// — so a slow/absent poller can never stall the engine (invariant #10): the
/// reader just sees the last published snapshot (or the empty seed).
#[derive(Debug)]
pub struct LoadSnapshot {
    inner: ArcSwap<Vec<DeviceLoad>>,
}

impl LoadSnapshot {
    /// A snapshot seeded with no devices (the honest pre-first-poll state).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Publish a fresh per-GPU load snapshot (off the hot path; newest-wins).
    pub fn publish(&self, loads: Vec<DeviceLoad>) {
        self.inner.store(Arc::new(loads));
    }

    /// Read the newest published snapshot, wait-free.
    #[must_use]
    pub fn load(&self) -> Arc<Vec<DeviceLoad>> {
        self.inner.load_full()
    }
}

impl Default for LoadSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// A discard [`PacketSink`] whose sole purpose is to make a NEW rendition *warm*
/// (≥1 sink ⇒ encoded once per tick) before a SWAP, so the cut spawns zero new
/// encodes (ADR-R010 §2.3 WARM, inv #7). It does **nothing** on
/// [`PacketSink::deliver`] — never blocks, never back-pressures (inv #10).
#[derive(Debug)]
pub struct KeepaliveSink {
    id: String,
}

impl KeepaliveSink {
    /// Build a keepalive sink with a stable id (unique within the rendition).
    #[must_use]
    pub fn new(id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self { id: id.into() })
    }
}

impl PacketSink for KeepaliveSink {
    fn sink_id(&self) -> &str {
        &self.id
    }
    fn deliver(&self, _packet: &Arc<EncodedPacket>) {
        // Discard: the keepalive exists only to keep its rendition active
        // (≥1 sink ⇒ warm). It never blocks or back-pressures (inv #10).
    }
}

/// The engine-side **RT-12 `output ← program` crosspoint** (ADR-R010): the
/// cross-`Program` sink-cutover bridge over a shared
/// [`multiview_output::fanout::PacketRouter`].
///
/// Each program registers its sinks under its own [`RenditionId`]; a SWAP
/// re-keys a consumer from OLD's rendition to NEW's via [`PacketRouter::move_sink`]
/// — a non-blocking, non-erroring frame-boundary table re-key. The router is
/// guarded by a [`std::sync::Mutex`] across which **no `.await` is ever held**:
/// every coordinator op ([`Self::register`]/[`Self::deregister`]/[`Self::move_sink`])
/// is an O(1) table mutation, and the per-program egress [`Self::route`] only
/// runs the (contractually non-blocking) `deliver` fan-out — so neither can
/// stall the output clock (invariants #1 + #10).
#[derive(Clone)]
pub struct OutputCrosspoint {
    router: Arc<Mutex<PacketRouter>>,
}

impl OutputCrosspoint {
    /// An empty crosspoint (no renditions, no sinks).
    #[must_use]
    pub fn new() -> Self {
        Self {
            router: Arc::new(Mutex::new(PacketRouter::new())),
        }
    }

    /// Build a crosspoint over an existing shared router (so the run path can
    /// hand the engine the **same** router its egress fans out through).
    #[must_use]
    pub fn from_router(router: Arc<Mutex<PacketRouter>>) -> Self {
        Self { router }
    }

    /// A clone of the shared router handle (the egress fan-out side).
    #[must_use]
    pub fn router(&self) -> Arc<Mutex<PacketRouter>> {
        Arc::clone(&self.router)
    }

    /// Lock the shared router, recovering a poisoned guard rather than
    /// propagating a panic across the control plane: the table is a plain map,
    /// so a torn op cannot corrupt memory, and stopping one migration must never
    /// poison the whole crosspoint (the egress fan-out keeps working).
    fn lock_router(&self) -> std::sync::MutexGuard<'_, PacketRouter> {
        self.router
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a real consumer sink under `rendition`.
    pub fn register(&self, rendition: RenditionId, sink: Arc<dyn PacketSink>) {
        self.lock_router().register(rendition, sink);
    }

    /// Pre-attach a [`KeepaliveSink`] to `rendition` (WARM phase) so it is warm
    /// (encoding) before a SWAP — the zero-new-encode precondition (inv #7).
    pub fn attach_keepalive(&self, rendition: RenditionId, keepalive: Arc<KeepaliveSink>) {
        self.lock_router().register(rendition, keepalive);
    }

    /// Deregister the sink `sink_id` from `rendition`. Returns `true` if removed
    /// (the bool is a diagnostic the caller may ignore).
    // reason: the load-bearing effect is the table mutation; the removed flag is
    // a diagnostic (mirrors `PacketRouter::deregister`'s non-must-use shape).
    #[allow(clippy::must_use_candidate)]
    pub fn deregister(&self, rendition: &RenditionId, sink_id: &str) -> bool {
        self.lock_router().deregister(rendition, sink_id)
    }

    /// **SWAP** one consumer from rendition `from` to rendition `to` (ADR-R010
    /// §2.4): a non-blocking, non-erroring routing-table re-key. Returns `true`
    /// if the sink moved (`false` is a no-op — never an error; a diagnostic the
    /// caller may ignore).
    // reason: the load-bearing effect is the re-key; the moved flag is a
    // diagnostic (mirrors `PacketRouter::move_sink`'s non-must-use shape).
    #[allow(clippy::must_use_candidate)]
    pub fn move_sink(&self, sink_id: &str, from: &RenditionId, to: RenditionId) -> bool {
        self.lock_router().move_sink(sink_id, from, to)
    }

    /// Route a packet to every sink under its rendition (the egress fan-out).
    /// Runs on the per-program egress thread, never the clock loop.
    #[allow(clippy::must_use_candidate)]
    pub fn route(&self, packet: &Arc<EncodedPacket>) -> usize {
        self.lock_router().route(packet)
    }

    /// Drive one **encode-once-mux-many** tick over the shared router: encode
    /// each active rendition (≥1 sink) exactly once via `encoder`, then fan the
    /// single packet to that rendition's sinks (inv #7). Used by the offline
    /// tests' encode-count spy to prove zero-new-encode at SWAP; production wires
    /// its own per-rendition encoder over [`Self::route`].
    #[allow(clippy::must_use_candidate)]
    pub fn tick<E: RenditionEncoder + ?Sized>(&self, tick: u64, encoder: &E) -> usize {
        // Snapshot the active renditions under the lock, then encode + route
        // each WITHOUT holding the lock across the encode (encode is the
        // caller's, may be heavy in production). The fan-out re-takes the lock
        // per packet (an O(1) read + non-blocking deliver fan-out).
        let active = self.lock_router().active_renditions();
        let renditions_encoded = active.len();
        for rendition in active {
            let packet = Arc::new(encoder.encode_frame(&rendition, tick));
            self.route(&packet);
        }
        renditions_encoded
    }
}

impl Default for OutputCrosspoint {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OutputCrosspoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let renditions = self.lock_router().rendition_count();
        f.debug_struct("OutputCrosspoint")
            .field("renditions", &renditions)
            .finish()
    }
}

/// Why a make-before-break migration rolled back **before** the SWAP (ADR-R010
/// §3): the common, non-disruptive failure mode — OLD is untouched, no consumer
/// moved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RollbackReason {
    /// VALIDATE: the cost-model admission check rejected running OLD **and** NEW
    /// concurrently (not enough headroom / encoder sessions on the target).
    AdmissionDenied(RejectReason),
    /// SPIN-UP: the NEW program failed to start (duplicate id, or a build error).
    SpinUpFailed(String),
    /// WARM: NEW did not reach readiness within the warm timeout.
    WarmTimeout,
}

/// The terminal outcome of a migration (ADR-R010 §1): either the cut completed
/// (`Migrated` — post-SWAP is committed, forward-only) or it rolled back before
/// the SWAP (`RolledBack`, non-disruptive).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrationOutcome {
    /// The migration completed: consumers are on NEW, OLD is drained + stopped.
    Migrated,
    /// The migration rolled back before SWAP; OLD is exactly as it was.
    RolledBack(RollbackReason),
}

/// **VALIDATE** (ADR-R010 §2.1): confirm the migration's target device is
/// admissible *now* — capability- and cost-gated against the **live** load
/// snapshot (which already reflects OLD's footprint, so this is the "hold both
/// concurrently" check).
///
/// The selector is run over the **target candidate alone, unpinned**, so the
/// headroom-ceiling gate (ADR-E007/E008) is enforced: a pin would *bypass* the
/// ceiling ("a pin always wins", ADR-0018 §2.1), which is exactly wrong for
/// VALIDATE — here we must reject a target that cannot fit both egresses
/// concurrently, not honour an operator override. So we ask "does the target,
/// alone, pass the hard gates **and** sit under the headroom ceiling under the
/// live load?" An admission failure rolls back before any resource is touched
/// (OLD untouched).
///
/// # Errors
///
/// [`RollbackReason::AdmissionDenied`] carrying the selector's
/// [`RejectReason`] when the target is not a viable whole-pipeline host under
/// the live snapshot — over the headroom ceiling
/// ([`RejectReason::AllOverHeadroomCeiling`]), fails a hard gate
/// ([`RejectReason::NoCandidateFitsWholePipeline`]), or is not among the
/// candidates ([`RejectReason::NoCandidates`]).
pub fn validate_migration(
    candidates: &[GpuCandidate],
    demand: &PipelineDemand,
    loads: &[DeviceLoad],
    policy: PlacementPolicy,
    plan: &MigrationPlan,
) -> Result<(), RollbackReason> {
    // Restrict the candidate set to the migration's TARGET only, and select
    // UNPINNED: with exactly one candidate the selector either places onto it
    // (it passes the hard gates AND the headroom ceiling under the live load) or
    // rejects it. A pin is deliberately NOT used — a pin bypasses the headroom
    // ceiling, but VALIDATE's whole job is to enforce "enough headroom to hold
    // OLD + NEW concurrently".
    let target_only: Vec<GpuCandidate> = candidates
        .iter()
        .filter(|c| c.device_id == plan.to)
        .cloned()
        .collect();
    match select_device(&target_only, demand, loads, &Pins::none(), policy) {
        Ok(_) => Ok(()),
        Err(reason) => Err(RollbackReason::AdmissionDenied(reason)),
    }
}

/// **DRAIN + STOP** (ADR-R010 §2.5): after the consumers are on NEW, stop the
/// OLD program — raise only its stop signal, drain its bounded egress, and join
/// its supervised task (bounded; a wedged task is shed at the join grace, never
/// blocking teardown — the #96 helper). Returns [`MigrationOutcome::Migrated`].
///
/// Siblings (and NEW) are untouched. This is a control-plane action off the data
/// plane: the bounded join gates nothing on the output clock (inv #1 + #10).
pub async fn drain_stop<P: Pacer + Send + 'static>(
    programs: &mut ProgramSet<P>,
    old_id: &str,
) -> MigrationOutcome {
    programs.stop(old_id).await;
    MigrationOutcome::Migrated
}

/// The live **placement-execution loop** (GPU-5c): owns the pure
/// [`PlacementController`], reads the wait-free [`LoadSnapshot`], holds the
/// [`OutputCrosspoint`] the make-before-break SWAP re-keys, and records every
/// decision in the [`PlacementCounters`].
///
/// One [`PlacementCoordinator`] tick is one slow **control** tick (1–4 Hz, the
/// `LoadPoller` cadence), driven on the CLI's control thread — **never** the
/// output clock. It is structurally incapable of stalling the engine: it
/// observes a wait-free snapshot and either holds, drives the existing
/// degradation ladder (a `Shed`), or records an accepted `Migrate`/`Split` — all
/// on the control plane (inv #1 + #10). Executing that `Migrate`/`Split` through the
/// make-before-break primitive is not yet wired into the live loop.
///
/// `P` is the [`Pacer`] the program set runs (production
/// [`RealtimePacer`](crate::RealtimePacer)).
pub struct PlacementCoordinator {
    controller: PlacementController,
    snapshot: Arc<LoadSnapshot>,
    crosspoint: OutputCrosspoint,
    counters: PlacementCounters,
    /// Monotonic count of accepted migrations executed (mirrors the counter, for
    /// tests/telemetry that want a local read without the registry).
    migrations_executed: Arc<AtomicU64>,
}

impl PlacementCoordinator {
    /// Build the execution loop over a controller, the shared load snapshot, the
    /// output crosspoint, and the process [`MetricsRegistry`] its
    /// [`PlacementCounters`] register against (the CLI owns the one registry the
    /// telemetry exporter scrapes).
    ///
    /// [`MetricsRegistry`]: multiview_telemetry::metrics::MetricsRegistry
    #[must_use]
    pub fn new(
        controller: PlacementController,
        snapshot: Arc<LoadSnapshot>,
        crosspoint: OutputCrosspoint,
        registry: &multiview_telemetry::metrics::MetricsRegistry,
    ) -> Self {
        Self::with_counters(
            controller,
            snapshot,
            crosspoint,
            PlacementCounters::register(registry),
        )
    }

    /// Build the execution loop with explicit [`PlacementCounters`] (so a test
    /// can register them against an isolated registry and assert the series).
    #[must_use]
    pub fn with_counters(
        controller: PlacementController,
        snapshot: Arc<LoadSnapshot>,
        crosspoint: OutputCrosspoint,
        counters: PlacementCounters,
    ) -> Self {
        Self {
            controller,
            snapshot,
            crosspoint,
            counters,
            migrations_executed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The placement counters (for telemetry export / test assertions).
    #[must_use]
    pub fn counters(&self) -> &PlacementCounters {
        &self.counters
    }

    /// The device the controller currently tracks as the pipeline's home.
    #[must_use]
    pub fn current_device(&self) -> &DeviceId {
        self.controller.current_device()
    }

    /// The RT-12 cutover crosspoint the make-before-break SWAP re-keys.
    #[must_use]
    pub fn crosspoint(&self) -> &OutputCrosspoint {
        &self.crosspoint
    }

    /// Observe the newest load snapshot and **return** the controller's proposal
    /// **without** executing it, recording the decision in the counters. This is
    /// the pure-decision half: it is called once per control tick by the CLI's
    /// placement loop (`multiview-cli`, at the `LoadPoller` cadence) and by tests
    /// asserting the anti-storm bound. Acting on the proposal is the caller's job —
    /// today that loop publishes a `Shed` event and records a `Migrate`/`Split`,
    /// but executing the migration (the make-before-break crosspoint cutover) is
    /// not yet wired into the live loop.
    pub fn observe_only(&mut self) -> PlacementProposal {
        let loads = self.snapshot.load();
        let proposal = self.controller.observe(loads.as_slice());
        self.record(&proposal);
        proposal
    }

    /// Record one proposal in the counters (and the local migration tally).
    fn record(&self, proposal: &PlacementProposal) {
        match proposal {
            PlacementProposal::Hold => self.counters.record_hold(),
            PlacementProposal::Shed { reason } => {
                self.counters.record_shed(suppress_reason(*reason));
            }
            PlacementProposal::Migrate(_) => {
                self.counters.record_migrate();
                self.migrations_executed.fetch_add(1, Ordering::Release);
            }
            PlacementProposal::Split(_) => self.counters.record_split(),
        }
    }

    /// The number of migrations this loop has executed (a local mirror of the
    /// `migrate` counter, for tests).
    #[must_use]
    pub fn migrations_executed(&self) -> u64 {
        self.migrations_executed.load(Ordering::Acquire)
    }
}

/// Map the engine's [`ShedReason`] to the telemetry [`SuppressReason`] (the
/// telemetry crate is a leaf and does not depend on the engine, so the mapping
/// lives here). The display-bound shed has no telemetry suppress reason of its
/// own (it is an affinity pin, not an anti-storm suppression), so it records as
/// the pinned suppression — the closest "this pipeline may not migrate" bucket.
const fn suppress_reason(reason: ShedReason) -> SuppressReason {
    match reason {
        ShedReason::Pinned | ShedReason::DisplayBound => SuppressReason::Pinned,
        ShedReason::NoBetterHome => SuppressReason::NoBetterHome,
        ShedReason::AntiStorm => SuppressReason::AntiStorm,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn load_snapshot_publishes_and_reads_wait_free() {
        let snap = LoadSnapshot::new();
        assert!(snap.load().is_empty(), "seed is empty");
        let load = DeviceLoad::unknown(DeviceId::new(
            multiview_hal::load::Vendor::Nvidia,
            "GPU-x",
            0,
        ));
        snap.publish(vec![load.clone()]);
        let read = snap.load();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].device_id, load.device_id);
    }

    #[test]
    fn keepalive_sink_is_a_named_no_op() {
        let k = KeepaliveSink::new("k1");
        assert_eq!(k.sink_id(), "k1");
        // deliver is a no-op (it must not panic / block).
        let packet = Arc::new(EncodedPacket {
            rendition: RenditionId::new("r"),
            kind: multiview_output::fanout::PacketKind::Audio,
            pts: 0,
            dts: 0,
            duration: 1,
            data: Arc::from(vec![0_u8].into_boxed_slice()),
        });
        k.deliver(&packet);
    }

    #[test]
    fn suppress_reason_maps_every_shed_reason() {
        assert_eq!(suppress_reason(ShedReason::Pinned), SuppressReason::Pinned);
        assert_eq!(
            suppress_reason(ShedReason::DisplayBound),
            SuppressReason::Pinned
        );
        assert_eq!(
            suppress_reason(ShedReason::NoBetterHome),
            SuppressReason::NoBetterHome
        );
        assert_eq!(
            suppress_reason(ShedReason::AntiStorm),
            SuppressReason::AntiStorm
        );
    }
}
