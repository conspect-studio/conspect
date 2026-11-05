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
        Arc,
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
    /// Explicit lifecycle generation.
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
}
/// Cloneable lock-free registry reader; contains no control-plane lock.
#[derive(Clone)]
pub struct SourceEventRegistry(Arc<Shared>);
impl SourceEventRegistry {
    /// Acquire current evidence. Retired sources disappear immediately. On loss,
    /// a bounded copy overlays atomic diagnostics without waiting for the worker.
    #[must_use]
    pub fn snapshot(&self, source: &str) -> Option<Arc<SourceEventSnapshot>> {
        if self.is_stopped() {
            return None;
        }
        let slots = self.0.slots.load();
        let generation = slots.get(source)?;
        let mut snapshot = generation.snapshot.load();
        let overflow = generation.overflow.load(Ordering::Acquire);
        let degraded = generation.degraded.load(Ordering::Acquire);
        if overflow != snapshot.queue_overflow || degraded != snapshot.degraded {
            let copy = Arc::make_mut(&mut snapshot);
            copy.queue_overflow = overflow;
            copy.degraded |= degraded;
        }
        if !generation.active.load(Ordering::Acquire) || self.is_stopped() {
            return None;
        }
        Some(snapshot)
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
impl SourceEventProducer {
    /// Validate ownership and payload bounds before attempting a bounded send.
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
                let _ = g
                    .overflow
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                        Some(v.saturating_add(1))
                    });
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
/// Explicit control-plane owner, with one isolated worker for all bounded sources.
pub struct SourceEventService {
    limits: ServiceLimits,
    shared: Arc<Shared>,
    tx: SyncSender<(Arc<Generation>, SourceEventObservation)>,
    worker: Option<JoinHandle<()>>,
}
impl SourceEventService {
    /// Start the isolated worker. Thread creation errors are returned to the owner.
    pub fn new(limits: ServiceLimits) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            slots: SwapCell::new(Arc::new(BTreeMap::new())),
            stopped: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::sync_channel(limits.queue_capacity);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("source-events".into())
            .spawn(move || run(worker_shared, rx, limits.tracker))?;
        Ok(Self {
            limits,
            shared,
            tx,
            worker: Some(worker),
        })
    }
    /// Read handle to pass explicitly to API/compositor; no global registry.
    #[must_use]
    pub fn registry(&self) -> SourceEventRegistry {
        SourceEventRegistry(Arc::clone(&self.shared))
    }
    /// Register/replace on the control plane. Old producers cannot write the new
    /// generation's slot, even if a queued old write finishes after replacement.
    pub fn register(
        &mut self,
        source: &str,
        epoch: u64,
    ) -> Result<SourceEventProducer, RegisterError> {
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
    /// Nonblocking retirement; worker exits after its bounded current operation
    /// or receive timeout. Never drains an adversarial producer queue.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Release);
    }
    /// Join only on a control/supervisor thread, never the engine. Tests should use
    /// this to reclaim the worker; Drop itself deliberately never blocks on join.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop();
        self.worker.take().map_or(Ok(()), JoinHandle::join)
    }
}
impl Drop for SourceEventService {
    fn drop(&mut self) {
        self.stop();
    }
}
fn run(
    shared: Arc<Shared>,
    rx: Receiver<(Arc<Generation>, SourceEventObservation)>,
    limits: TrackerLimits,
) {
    struct Finished(Arc<Shared>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.stopped.store(true, Ordering::Release);
            self.0.finished.store(true, Ordering::Release);
        }
    }
    let _finished = Finished(Arc::clone(&shared));
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
            g.snapshot.store(snapshot);
        }
    }
}
