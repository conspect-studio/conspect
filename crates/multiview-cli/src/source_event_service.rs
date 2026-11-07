//! Shared, bounded source-event runtime (ADR-0183), with no automation effects.
//!
//! Construct/register/replace on the control plane; share `registry()` with API
//! and compositor, and move each producer into its ingest owner. Replacement and
//! discontinuity require a strictly newer explicit epoch AND a fresh normalizer
//! mapping/adapter. No receipt clock is interpreted as presentation time.
//!
//! Slots retain epoch floors for the service lifetime: removed identities are not
//! evicted to admit others. There is no same-epoch pruning, so forgotten events
//! cannot replay; capacity rejects and degrades instead. Restart replay protection
//! requires the owner to persist epochs/idempotency policy before enabling effects.
//! Snapshots are observational only, never authorization/automation eligibility.
//! An already acquired Arc cannot be revoked: delayed consumers must revalidate
//! source/epoch against a current registry snapshot before using it.
use multiview_core::source_event::{
    EventOrigin, ObservationOutcome, SourceEvent, SourceEventObservation, SourceEventTracker,
    TrackerDiagnostics, TrackerLimits,
};
use multiview_framestore::SwapCell;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Explicit finite runtime bounds; allocation and registration are off-clock.
#[derive(Debug, Clone, Copy)]
pub struct ServiceLimits {
    /// Distinct source identities over this service lifetime (includes tombstones).
    pub max_sources: usize,
    /// Total queued observations across all sources. Zero is a rendezvous queue.
    pub queue_capacity: usize,
    /// Per-source evidence and string bounds.
    pub tracker: TrackerLimits,
}
impl Default for ServiceLimits {
    fn default() -> Self {
        Self {
            max_sources: 64,
            queue_capacity: 256,
            tracker: TrackerLimits::default(),
        }
    }
}
/// Immutable evidence, not projected against a receipt-time clock.
#[derive(Debug, Clone)]
pub struct SourceEventSnapshot {
    /// Registered identity.
    pub source_id: String,
    /// Monotonic service epoch, distinct from frame lifecycle/mapping identity.
    pub epoch: u64,
    /// Bounded retained events; project using the actual presented frame clock.
    pub events: Vec<SourceEvent>,
    /// Tracker rejection/retention counters.
    pub diagnostics: TrackerDiagnostics,
    /// Latched until replacement: lost or invalid evidence must fail closed.
    pub degraded: bool,
    /// Saturating queue loss count for this generation.
    pub queue_overflow: u64,
}
struct Generation {
    source: String,
    epoch: u64,
    active: AtomicBool,
    overflow: AtomicU64,
    degraded: AtomicBool,
    snapshot: SwapCell<Arc<SourceEventSnapshot>>,
}
struct Shared {
    slots: SwapCell<Arc<BTreeMap<String, Arc<Generation>>>>,
    stopped: AtomicBool,
    finished: AtomicBool,
    /// Shared with [`RegistrationController`]/[`SourceEventProducer`] so a
    /// registry reader can submit an administrator-injected test observation
    /// (ADR-0183 operator test aid) through the SAME bounded worker queue and
    /// per-generation epoch check as ordinary evidence, without handing out a
    /// full [`SourceEventProducer`] capability.
    test_tx: SyncSender<(Arc<Generation>, SourceEventObservation)>,
    max_string_bytes: usize,
    #[cfg(test)]
    publication_hook: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}
/// Constant-size acquired view. Loss metadata is sampled separately from immutable
/// evidence, so repeated fail-closed reads never copy the retained event vector.
#[derive(Debug, Clone)]
pub struct SourceEventRead {
    /// Shared immutable worker evidence; its loss counters may lag this read.
    pub evidence: Arc<SourceEventSnapshot>,
    /// Registered service epoch sampled for this read.
    pub epoch: u64,
    /// Latest sampled fail-closed flag; use this rather than evidence.degraded.
    pub degraded: bool,
    /// Latest sampled saturating queue loss count.
    pub queue_overflow: u64,
}
impl std::ops::Deref for SourceEventRead {
    type Target = SourceEventSnapshot;
    fn deref(&self) -> &Self::Target {
        &self.evidence
    }
}
/// Cloneable lock-free registry reader; contains no control-plane lock.
#[derive(Clone)]
pub struct SourceEventRegistry(Arc<Shared>);
impl SourceEventRegistry {
    /// Acquire evidence and immediate loss metadata without copying event state.
    #[must_use]
    pub fn snapshot(&self, source: &str) -> Option<SourceEventRead> {
        if self.is_stopped() {
            return None;
        }
        let slots = self.0.slots.load();
        let generation = slots.get(source)?;
        let evidence = generation.snapshot.load();
        let queue_overflow = generation.overflow.load(Ordering::Acquire);
        let degraded = generation.degraded.load(Ordering::Acquire) || evidence.degraded;
        if !generation.active.load(Ordering::Acquire) || self.is_stopped() {
            return None;
        }
        Some(SourceEventRead {
            evidence,
            epoch: generation.epoch,
            degraded,
            queue_overflow,
        })
    }
    /// Validate a delayed view at use time. This is an identity check, not an
    /// authorization lease: retirement may still race subsequent consumer work.
    #[must_use]
    pub fn is_current(&self, acquired: &SourceEventRead) -> bool {
        self.snapshot(&acquired.source_id)
            .is_some_and(|current| current.epoch == acquired.epoch)
    }
    /// Stop was requested; no snapshots remain usable from this registry.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }
    /// Worker has exited (useful for supervising nonblocking shutdown).
    #[must_use]
    pub fn worker_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }
    /// Operator test aid (ADR-0183 item 5): submit a synthetic observation
    /// against this source's CURRENT active generation and epoch, through the
    /// SAME bounded worker queue and validation [`SourceEventProducer::try_observe`]
    /// uses, so a test event clears/overlays/automates exactly like real
    /// evidence and never bypasses capacity or payload bounds. Requires an
    /// already-active (live or previously connected) generation — this never
    /// allocates a new source identity or epoch, so it cannot be used to probe
    /// unknown source ids.
    ///
    /// # Errors
    /// `None` when the source has no active generation (unknown or retired).
    #[must_use]
    pub fn inject_test_observation(
        &self,
        source: &str,
        boundary: crate::source_event_test_inject::TestBoundary,
    ) -> Option<SubmitOutcome> {
        let slots = self.0.slots.load();
        let generation = slots.get(source)?;
        if !generation.active.load(Ordering::Acquire) || self.is_stopped() {
            return None;
        }
        let epoch = generation.epoch;
        let observation = boundary.into_observation(source.to_owned(), epoch);
        if observation.key.event_id.is_empty()
            || observation.key.event_id.len() > self.0.max_string_bytes
        {
            generation.degraded.store(true, Ordering::Release);
            return Some(SubmitOutcome::Invalid);
        }
        Some(
            match self
                .0
                .test_tx
                .try_send((Arc::clone(generation), observation))
            {
                Ok(()) => SubmitOutcome::Queued,
                Err(TrySendError::Full(_)) => {
                    generation.degraded.store(true, Ordering::Release);
                    SubmitOutcome::Overflow
                }
                Err(TrySendError::Disconnected(_)) => SubmitOutcome::Retired,
            },
        )
    }
}
/// Nonblocking submission result. Queued is not proof of accepted evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Accepted into the bounded worker queue.
    Queued,
    /// Queue full; generation permanently degraded.
    Overflow,
    /// Handle replaced, dropped or service stopped.
    Retired,
    /// Observation does not belong to this source.
    WrongSource,
    /// Observation does not belong to this epoch.
    WrongEpoch,
    /// Payload exceeds configured string bounds.
    Invalid,
}
/// Single lifecycle owner. Share with `Arc` if necessary; final drop retires it.
/// No send waits for worker progress and no drop joins a worker.
pub struct SourceEventProducer {
    generation: Arc<Generation>,
    shared: Arc<Shared>,
    tx: SyncSender<(Arc<Generation>, SourceEventObservation)>,
    max_string_bytes: usize,
}
/// Non-owning generation-specific evidence-loss signal; never retires a producer.
#[derive(Clone)]
pub struct EvidenceLossHandle(std::sync::Weak<Generation>);
impl EvidenceLossHandle {
    /// Mark only the original generation incomplete, if it still exists.
    pub fn mark_degraded(&self) {
        if let Some(generation) = self.0.upgrade() {
            generation.degraded.store(true, Ordering::Release);
        }
    }
}

impl SourceEventProducer {
    /// Share a non-owning loss signal with a connection-scoped metadata observer.
    #[must_use]
    pub fn loss_handle(&self) -> EvidenceLossHandle {
        EvidenceLossHandle(Arc::downgrade(&self.generation))
    }

    /// Latch incomplete upstream evidence without allocating or enqueueing a cue.
    /// A retired handle can only affect its retired generation, never a replacement.
    pub fn mark_degraded(&self) {
        self.generation.degraded.store(true, Ordering::Release);
    }

    /// Revoke this generation immediately without joining the worker.
    /// Already queued work remains isolated in this generation's retired slot.
    pub fn retire(&self) {
        self.generation.active.store(false, Ordering::Release);
    }

    /// Validate ownership and payload bounds before attempting a bounded send.
    #[must_use]
    pub fn try_observe(&self, observation: SourceEventObservation) -> SubmitOutcome {
        let g = &self.generation;
        if self.shared.stopped.load(Ordering::Acquire) || !g.active.load(Ordering::Acquire) {
            return SubmitOutcome::Retired;
        }
        if observation.key.source_id != g.source {
            return SubmitOutcome::WrongSource;
        }
        if observation.key.epoch != g.epoch {
            return SubmitOutcome::WrongEpoch;
        }
        let vendor = match &observation.key.origin {
            EventOrigin::VendorMarker { vendor } => Some(vendor.as_str()),
            _ => None,
        };
        if observation.key.event_id.is_empty()
            || [
                Some(observation.key.event_id.as_str()),
                observation.asset_id.as_deref(),
                observation.correlation_id.as_deref(),
                vendor,
            ]
            .into_iter()
            .flatten()
            .any(|s| s.len() > self.max_string_bytes)
        {
            g.degraded.store(true, Ordering::Release);
            return SubmitOutcome::Invalid;
        }
        match self.tx.try_send((Arc::clone(g), observation)) {
            Ok(()) => SubmitOutcome::Queued,
            Err(TrySendError::Full(_)) => {
                g.degraded.store(true, Ordering::Release);
                let mut count = g.overflow.load(Ordering::Acquire);
                while count != u64::MAX {
                    match g.overflow.compare_exchange_weak(
                        count,
                        count + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break,
                        Err(actual) => count = actual,
                    }
                }
                SubmitOutcome::Overflow
            }
            Err(TrySendError::Disconnected(_)) => SubmitOutcome::Retired,
        }
    }
}
impl Drop for SourceEventProducer {
    fn drop(&mut self) {
        self.generation.active.store(false, Ordering::Release);
    }
}
/// Registration failures never retire an existing valid generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// Service has stopped.
    Stopped,
    /// Identity empty or too long.
    InvalidSource,
    /// Bounded lifetime source table full.
    Capacity,
    /// Epoch did not strictly increase, including after producer drop.
    StaleEpoch,
}
/// Cloneable, bounded control/ingest-supervisor registration capability.
/// The mutex serializes table replacement and epoch checks; never call register
/// on an output-clock thread. Readers and observation submission never take it.
#[derive(Clone)]
pub struct RegistrationController {
    limits: ServiceLimits,
    shared: Arc<Shared>,
    tx: SyncSender<(Arc<Generation>, SourceEventObservation)>,
    registration: Arc<Mutex<()>>,
}
/// Explicit control-plane owner, with one isolated worker for all bounded sources.
pub struct SourceEventService {
    controller: RegistrationController,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}
impl SourceEventService {
    /// Start the isolated worker.
    ///
    /// # Errors
    /// Returns the OS thread creation error if the worker cannot start.
    pub fn new(limits: ServiceLimits) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(limits.queue_capacity);
        let shared = Arc::new(Shared {
            slots: SwapCell::new(Arc::new(BTreeMap::new())),
            stopped: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            test_tx: tx.clone(),
            max_string_bytes: limits.tracker.max_string_bytes,
            #[cfg(test)]
            publication_hook: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("source-events".into())
            .spawn(move || run(&worker_shared, &rx, limits.tracker))?;
        Ok(Self {
            controller: RegistrationController {
                limits,
                shared: Arc::clone(&shared),
                tx,
                registration: Arc::new(Mutex::new(())),
            },
            shared,
            worker: Some(worker),
        })
    }
    /// Read handle to pass explicitly to API/compositor; no global registry.
    #[must_use]
    pub fn registry(&self) -> SourceEventRegistry {
        SourceEventRegistry(Arc::clone(&self.shared))
    }
    /// Obtain an off-clock registration handle for ingest/reconnect supervisors.
    #[must_use]
    pub fn registration_controller(&self) -> RegistrationController {
        self.controller.clone()
    }
    /// Register a supplied strictly monotonic service epoch (not frame generation).
    ///
    /// # Errors
    /// Rejects stopped services, invalid identities, stale epochs and exhausted capacity.
    pub fn register(&self, source: &str, epoch: u64) -> Result<SourceEventProducer, RegisterError> {
        self.controller.register(source, epoch)
    }
    /// Nonblocking retirement; worker exits after its bounded current operation
    /// or receive timeout. Never drains an adversarial producer queue.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Release);
    }
    /// Join only on a control/supervisor thread, never the engine. Tests should use
    /// this to reclaim the worker; Drop itself deliberately never blocks on join.
    ///
    /// # Errors
    /// Returns the worker panic payload if the worker panicked.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop();
        self.worker.take().map_or(Ok(()), JoinHandle::join)
    }
}
impl RegistrationController {
    /// Register/replace on the control plane. Old producers cannot write the new
    /// generation's slot, even if a queued old write finishes after replacement.
    ///
    /// # Errors
    /// Rejects stopped services, invalid identities, stale epochs and exhausted capacity.
    pub fn register(&self, source: &str, epoch: u64) -> Result<SourceEventProducer, RegisterError> {
        let _registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(RegisterError::Stopped);
        }
        if source.is_empty() || source.len() > self.limits.tracker.max_string_bytes {
            return Err(RegisterError::InvalidSource);
        }
        let old = self.shared.slots.load();
        if let Some(g) = old.get(source) {
            if epoch <= g.epoch {
                return Err(RegisterError::StaleEpoch);
            }
        } else if old.len() >= self.limits.max_sources {
            return Err(RegisterError::Capacity);
        }
        let g = Arc::new(Generation {
            source: source.into(),
            epoch,
            active: AtomicBool::new(true),
            overflow: AtomicU64::new(0),
            degraded: AtomicBool::new(false),
            snapshot: SwapCell::new(Arc::new(SourceEventSnapshot {
                source_id: source.into(),
                epoch,
                events: Vec::new(),
                diagnostics: TrackerDiagnostics::default(),
                degraded: false,
                queue_overflow: 0,
            })),
        });
        let mut next = (*old).clone();
        if let Some(previous) = next.insert(source.into(), Arc::clone(&g)) {
            previous.active.store(false, Ordering::Release);
        }
        self.shared.slots.store(Arc::new(next));
        Ok(SourceEventProducer {
            generation: g,
            shared: Arc::clone(&self.shared),
            tx: self.tx.clone(),
            max_string_bytes: self.limits.tracker.max_string_bytes,
        })
    }
}
impl Drop for SourceEventService {
    fn drop(&mut self) {
        self.stop();
    }
}
fn run(
    shared: &Arc<Shared>,
    rx: &Receiver<(Arc<Generation>, SourceEventObservation)>,
    limits: TrackerLimits,
) {
    struct Finished(Arc<Shared>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.stopped.store(true, Ordering::Release);
            self.0.finished.store(true, Ordering::Release);
        }
    }
    let _finished = Finished(Arc::clone(shared));
    let mut trackers: BTreeMap<String, (u64, SourceEventTracker)> = BTreeMap::new();
    while !shared.stopped.load(Ordering::Acquire) {
        let (g, observation) = match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(item) => item,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if !g.active.load(Ordering::Acquire) {
            continue;
        }
        let entry = trackers
            .entry(g.source.clone())
            .or_insert_with(|| (g.epoch, SourceEventTracker::new(limits)));
        if entry.0 != g.epoch {
            *entry = (g.epoch, SourceEventTracker::new(limits));
        }
        let tracker = &mut entry.1;
        if matches!(
            tracker.observe(observation),
            ObservationOutcome::Conflict
                | ObservationOutcome::CapacityExceeded
                | ObservationOutcome::Invalid
        ) {
            g.degraded.store(true, Ordering::Release);
        }
        let snapshot = Arc::new(SourceEventSnapshot {
            source_id: g.source.clone(),
            epoch: g.epoch,
            events: tracker.events().cloned().collect(),
            diagnostics: *tracker.diagnostics(),
            degraded: g.degraded.load(Ordering::Acquire),
            queue_overflow: g.overflow.load(Ordering::Acquire),
        });
        // The slot belongs exclusively to this generation. An old writer can
        // never reach its replacement, even across this check/store race.
        if g.active.load(Ordering::Acquire) && !shared.stopped.load(Ordering::Acquire) {
            #[cfg(test)]
            {
                let hook = shared.publication_hook.lock().unwrap().take();
                if let Some((entered, release)) = hook {
                    entered.wait();
                    release.wait();
                }
            }
            g.snapshot.store(snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_core::source_event::{EventBoundary, SourceEventKey};
    use std::sync::Barrier;

    fn observation(epoch: u64) -> SourceEventObservation {
        SourceEventObservation {
            key: SourceEventKey {
                source_id: "a".into(),
                epoch,
                origin: EventOrigin::Scte35,
                event_id: "cue".into(),
            },
            revision: 0,
            boundary: EventBoundary::Start {
                pts_ns: Some(10),
                duration_ns: None,
            },
            correlation_id: None,
            asset_id: None,
            descriptor: None,
        }
    }
    // Pause after the worker's final active check, exercising the actual stale
    // publication window rather than relying on scheduler timing.
    fn pause_publication(service: &SourceEventService) -> (Arc<Barrier>, Arc<Barrier>) {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *service.shared.publication_hook.lock().unwrap() = Some((entered.clone(), release.clone()));
        (entered, release)
    }
    #[test]
    fn publication_after_replacement_is_isolated() {
        let service = SourceEventService::new(ServiceLimits::default()).unwrap();
        let view = service.registry();
        let old = service.register("a", 1).unwrap();
        let (entered, release) = pause_publication(&service);
        assert_eq!(old.try_observe(observation(1)), SubmitOutcome::Queued);
        entered.wait();
        let new = service.registration_controller().register("a", 2).unwrap();
        let current = view.snapshot("a").unwrap();
        release.wait();
        service.shutdown().unwrap();
        assert!(current.events.is_empty());
        assert_eq!(current.epoch, 2);
        assert!(new.generation.snapshot.load().events.is_empty());
        assert_eq!(old.generation.snapshot.load().events.len(), 1);
    }
    #[test]
    fn publication_after_producer_drop_remains_retired() {
        let service = SourceEventService::new(ServiceLimits::default()).unwrap();
        let view = service.registry();
        let producer = service.register("a", 1).unwrap();
        let generation = producer.generation.clone();
        let (entered, release) = pause_publication(&service);
        assert_eq!(producer.try_observe(observation(1)), SubmitOutcome::Queued);
        entered.wait();
        drop(producer);
        assert!(view.snapshot("a").is_none());
        release.wait();
        service.shutdown().unwrap();
        assert_eq!(generation.snapshot.load().events.len(), 1);
        assert!(!generation.active.load(Ordering::Acquire));
    }
    #[test]
    fn repeated_overflow_reads_reuse_evidence_arc() {
        let service = SourceEventService::new(ServiceLimits {
            queue_capacity: 1,
            ..ServiceLimits::default()
        })
        .unwrap();
        let view = service.registry();
        let producer = service.register("a", 1).unwrap();
        assert_eq!(producer.try_observe(observation(1)), SubmitOutcome::Queued);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while view.snapshot("a").unwrap().events.is_empty() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        let (entered, release) = pause_publication(&service);
        assert_eq!(producer.try_observe(observation(1)), SubmitOutcome::Queued);
        entered.wait();
        assert_eq!(producer.try_observe(observation(1)), SubmitOutcome::Queued);
        let first = view.snapshot("a").unwrap();
        assert_eq!(first.events.len(), 1);
        for count in 1..=100 {
            assert_eq!(
                producer.try_observe(observation(1)),
                SubmitOutcome::Overflow
            );
            let current = view.snapshot("a").unwrap();
            assert!(Arc::ptr_eq(&first.evidence, &current.evidence));
            assert!(current.degraded);
            assert_eq!(current.queue_overflow, count);
        }
        release.wait();
        service.shutdown().unwrap();
    }
}

// R3 stress fixture: test-only access is needed to pause publication without polling.
#[cfg(test)]
mod source_event_stress {
    use super::*;
    use crate::source_event_runtime::Runtime;
    use multiview_compositor::{
        blend::LinearRgba,
        pipeline::{CanvasColor, Nv12Image},
    };
    use multiview_core::{
        color::ColorInfo,
        layout::{Canvas, Cell, Layout},
        source_event::{EventBoundary, SourceEventKey},
        time::{MediaTime, Rational},
    };
    use multiview_engine::{
        clock::{ManualTimeSource, TimeSource},
        CompositorDrive, OutputClock,
    };
    use multiview_framestore::tile::SourceFrameTime;
    use std::{collections::HashMap, sync::Barrier, time::Instant};

    const SOURCES: usize = 64;
    const EVENTS: usize = 256;
    const TICKS: u64 = 32;

    fn cue(source: &str, epoch: u64, event: usize) -> SourceEventObservation {
        SourceEventObservation {
            key: SourceEventKey {
                source_id: source.into(),
                epoch,
                origin: EventOrigin::Scte35,
                event_id: format!("cue-{event:03}"),
            },
            revision: 0,
            boundary: EventBoundary::Start {
                pts_ns: Some(0),
                duration_ns: Some(60_000_000_000),
            },
            correlation_id: None,
            asset_id: Some("stress-asset".into()),
            descriptor: None,
        }
    }

    /// A bounded watchdog catches deadlocks, not performance regressions. No sleeps
    /// or polling: publication and flood interleavings use explicit barriers.
    #[test]
    fn r3_max_evidence_flood_stalled_reader_keeps_engine_frames() {
        let (done, result) = mpsc::sync_channel(1);
        let task = thread::spawn(move || {
            let outcome = std::panic::catch_unwind(stress);
            let _ = done.send(outcome);
        });
        match result
            .recv_timeout(Duration::from_secs(90))
            .expect("R3 stress deadlocked or exceeded bounded test duration")
        {
            Ok(()) => task.join().unwrap(),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the R3 deadlock-vs-starvation stress fixture needs one coherent \
                  sequence of setup, flood and barrier-synchronized sampling; \
                  splitting it would scatter the shared state across helpers"
    )]
    fn stress() {
        let limits = ServiceLimits::default();
        assert_eq!(limits.max_sources, SOURCES);
        assert_eq!(limits.tracker.max_events, EVENTS);
        assert_eq!(limits.queue_capacity, 256);
        let runtime = Runtime::new().unwrap();
        let reader = runtime.reader();
        let registry = reader.registry();
        let mut selected = HashMap::new();
        let mut producers = Vec::new();
        let mut cells = Vec::new();
        for source in 0..SOURCES {
            let id = format!("source-{source:02}");
            let lease = runtime.activate(&id, 1).unwrap();
            let (context, producer) = lease.connect().unwrap().register(1).unwrap();
            selected.insert(
                id.clone(),
                SourceFrameTime {
                    pts: MediaTime::from_nanos(1_000_000_000),
                    generation: 1,
                    event_clock: Some(context.clock),
                },
            );
            cells.push(Cell {
                x: f32::from(u8::try_from(source % 8).unwrap()) / 8.0,
                y: f32::from(u8::try_from(source / 8).unwrap()) / 8.0,
                w: 0.125,
                h: 0.125,
                source: Some(id),
                ..Cell::default()
            });
            producers.push(producer);
        }
        assert!(runtime.activate("source-over-capacity", 1).is_none());
        assert_eq!(registry.0.slots.load().len(), SOURCES);
        let layout = Layout {
            name: "R3-64-tile".into(),
            canvas: Canvas {
                width: 320,
                height: 192,
                fps_num: 60000,
                fps_den: 1001,
            },
            cells,
        };
        let slate = Nv12Image::solid(
            40,
            24,
            16,
            128,
            128,
            ColorInfo::default().resolve_defaults(320, 192),
        )
        .unwrap();
        let drive = CompositorDrive::new(
            Arc::new(layout),
            HashMap::new(),
            slate,
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
        )
        .unwrap();
        let fake = ManualTimeSource::new();
        let mut clock = OutputClock::new(Rational::FPS_59_94).unwrap();
        let mut emitted = 0_u64;
        let mut run = |expect_overlays: usize, rendezvous: Option<&Barrier>| {
            let mut costs = Vec::new();
            let mut sample_costs = Vec::new();
            for _ in 0..TICKS {
                if let Some(barrier) = rendezvous {
                    barrier.wait();
                }
                // Independent integer oracle, nearest rounding, not pts_at().
                let n = i128::from(emitted) * 1_000_000_000 * 1001;
                let expected = i64::try_from((n + 30000) / 60000).unwrap();
                fake.set(expected);
                let start = Instant::now();
                let tick = clock.tick();
                assert_eq!(tick.index, emitted);
                assert_eq!(tick.pts.as_nanos(), fake.now_nanos());
                let sampling = Instant::now();
                let overlays = reader.sample(&selected);
                sample_costs.push(sampling.elapsed());
                assert_eq!(overlays.len(), expect_overlays);
                let frame = drive.compose(tick).unwrap();
                assert_eq!(frame.tick, tick);
                assert_eq!((frame.canvas.width(), frame.canvas.height()), (320, 192));
                assert_eq!(frame.canvas.y_plane().len(), 320 * 192);
                costs.push(start.elapsed());
                emitted += 1;
                if let Some(barrier) = rendezvous {
                    barrier.wait();
                }
            }
            costs.sort();
            sample_costs.sort();
            let mid = usize::try_from(TICKS).unwrap() / 2;
            (sample_costs[mid], costs[mid], *costs.last().unwrap())
        };
        let baseline = run(0, None);
        // Seed through the real bounded tracker off-clock. Direct publication is
        // fixture setup only: public APIs have no flush/ack; sleeps are forbidden.
        for p in &producers {
            let mut tracker = SourceEventTracker::new(limits.tracker);
            for event in 0..EVENTS {
                tracker.observe(cue(&p.generation.source, p.generation.epoch, event));
            }
            assert_eq!(tracker.events().count(), EVENTS);
            p.generation.snapshot.store(Arc::new(SourceEventSnapshot {
                source_id: p.generation.source.clone(),
                epoch: p.generation.epoch,
                events: tracker.events().cloned().collect(),
                diagnostics: *tracker.diagnostics(),
                degraded: false,
                queue_overflow: 0,
            }));
        }
        let retained: Vec<_> = producers
            .iter()
            .map(|p| registry.snapshot(&p.generation.source).unwrap())
            .collect();
        assert_eq!(
            retained.iter().map(|r| r.events.len()).sum::<usize>(),
            SOURCES * EVENTS
        );
        let loaded = run(SOURCES, None);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *registry.0.publication_hook.lock().unwrap() = Some((entered.clone(), release.clone()));
        assert_eq!(
            producers[0].try_observe(cue("source-00", 1, 0)),
            SubmitOutcome::Queued
        );
        entered.wait(); // worker holds exactly one in-flight observation
        for _ in 0..limits.queue_capacity {
            assert_eq!(
                producers[0].try_observe(cue("source-00", 1, 0)),
                SubmitOutcome::Queued
            );
        }
        // Hold all snapshots and stop all API consumption. The worker cannot drain
        // the full queue; every subsequent submit must reject, never enqueue.
        for p in &producers {
            assert_eq!(
                p.try_observe(cue(&p.generation.source, p.generation.epoch, 0)),
                SubmitOutcome::Overflow
            );
        }
        let start = Arc::new(Barrier::new(2));
        let flood_start = start.clone();
        let flood = thread::spawn(move || {
            for _ in 0..TICKS {
                flood_start.wait();
                for p in &producers {
                    for event in 0..128 {
                        assert_eq!(
                            p.try_observe(cue(&p.generation.source, p.generation.epoch, event)),
                            SubmitOutcome::Overflow
                        );
                    }
                }
                flood_start.wait();
            }
            producers
        });
        // Barriers are test orchestration outside the timed engine call. Each
        // frame shares a phase with 8192 attempted submissions, not a scheduler race.
        let saturated = run(0, Some(&start));
        let producers = flood.join().unwrap();
        for (p, old) in producers.iter().zip(&retained) {
            let current = registry.snapshot(&p.generation.source).unwrap();
            assert!(current.degraded);
            assert_eq!(current.queue_overflow, 4097);
            assert_eq!(current.events.len(), EVENTS);
            assert!(Arc::ptr_eq(&current.evidence, &old.evidence));
        }
        assert_eq!(registry.0.slots.load().len(), SOURCES);
        assert_eq!(emitted, 3 * TICKS);
        // Evidence-based budget signal, not a host-speed assertion. Includes a
        // real software 64-tile slate composition, not encoder/muxer or overlays.
        let budget = Duration::from_nanos(16_683_333);
        eprintln!("R3 stress: baseline sample/total median/max={baseline:?}; max-evidence={loaded:?}; saturated={saturated:?}; 59.94fps budget={budget:?}; max-evidence median budget issue={}; frames={emitted}; retained events={}; queue={}+1 in-flight; exact rejections={}", loaded.1 > budget, SOURCES * EVENTS, limits.queue_capacity, SOURCES * 4097);
        release.wait();
        runtime.shutdown().unwrap();
        assert!(registry.is_stopped());
        // Retained immutable snapshots remain bounded and valid after shutdown.
        assert_eq!(
            retained.iter().map(|r| r.events.len()).sum::<usize>(),
            SOURCES * EVENTS
        );
    }
}
