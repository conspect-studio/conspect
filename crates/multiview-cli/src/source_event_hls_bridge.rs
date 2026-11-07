//! Connection-local, bounded original-manifest evidence transport.
//!
//! This bridge never registers or retires a service producer. Its receiver must
//! remain with the native connection that created it; only that owner may bind
//! evidence to its real mapping epoch. Vendor evidence never acquires timing.
use crate::hls_event_observer::{HlsEventObserver, HlsEvidenceSink};
use crate::source_event_service::{EvidenceLossHandle, SourceEventProducer, SubmitOutcome};
use multiview_core::source_event::{EventBoundary, EventOrigin, SourceEventObservation};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{sync_channel, Receiver, SyncSender},
    Arc, Mutex,
};

struct Guard {
    active: AtomicBool,
    loss: AtomicBool,
    loss_handle: Mutex<Option<EvidenceLossHandle>>,
}
impl Guard {
    fn signal_loss(&self) {
        self.loss.store(true, Ordering::Release);
        if let Some(handle) = self
            .loss_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            handle.mark_degraded();
        }
    }
}
struct Sink {
    source: String,
    fetch_generation: u64,
    tx: SyncSender<SourceEventObservation>,
    guard: Arc<Guard>,
}
impl HlsEvidenceSink for Sink {
    fn try_observe(&mut self, observation: SourceEventObservation) -> bool {
        let valid = self.guard.active.load(Ordering::Acquire)
            && observation.key.source_id == self.source
            && observation.key.epoch == self.fetch_generation
            && matches!(observation.key.origin, EventOrigin::VendorMarker { .. })
            && matches!(
                observation.boundary,
                EventBoundary::MarkerObserved | EventBoundary::MarkerAbsent
            );
        if !valid || self.tx.try_send(observation).is_err() {
            self.mark_degraded();
            return false;
        }
        true
    }
    fn mark_degraded(&mut self) {
        self.guard.signal_loss();
    }
    fn retire(&mut self) {
        self.guard.active.store(false, Ordering::Release);
        self.guard.signal_loss();
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.retire();
    }
}

/// Unique receiver capability, owned by exactly one native fetch connection.
pub struct HlsBridgeReceiver {
    source: String,
    fetch_generation: u64,
    rx: Receiver<SourceEventObservation>,
    guard: Arc<Guard>,
    epoch: Option<u64>,
    capacity: usize,
}

/// Allocate a bounded bridge before opening the fetcher, off the output clock.
/// `fetch_generation` is an opaque connection-local identity, NOT a service epoch.
#[must_use]
pub fn channel(
    source: String,
    fetch_generation: u64,
    capacity: usize,
) -> (HlsEventObserver, HlsBridgeReceiver) {
    let (tx, rx) = sync_channel(capacity);
    let guard = Arc::new(Guard {
        active: AtomicBool::new(true),
        loss: AtomicBool::new(false),
        loss_handle: Mutex::new(None),
    });
    let sink = Sink {
        source: source.clone(),
        fetch_generation,
        tx,
        guard: Arc::clone(&guard),
    };
    let observer = HlsEventObserver::new(source.clone(), fetch_generation, sink, capacity, 4096);
    (
        observer,
        HlsBridgeReceiver {
            source,
            fetch_generation,
            rx,
            guard,
            epoch: None,
            capacity,
        },
    )
}
impl HlsBridgeReceiver {
    /// Bind validated original observations to the SAME connection's native
    /// producer. The caller must prove connection ownership (never look up a
    /// producer by source name). Call only once a mapping is ready; early loss
    /// remains latched until then. An epoch change rejects this fetch generation.
    /// No callback, blocking send, or independent service registration occurs.
    pub fn drain_into(&mut self, source: &str, service_epoch: u64, producer: &SourceEventProducer) {
        if source != self.source || self.epoch.is_some_and(|epoch| epoch != service_epoch) {
            self.guard.active.store(false, Ordering::Release);
            producer.mark_degraded();
            return;
        }
        self.epoch = Some(service_epoch);
        {
            let mut handle = self
                .guard
                .loss_handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if handle.is_none() {
                *handle = Some(producer.loss_handle());
            }
        }
        if !self.guard.active.load(Ordering::Acquire) {
            producer.mark_degraded();
            return;
        }
        if self.guard.loss.load(Ordering::Acquire) {
            producer.mark_degraded();
        }
        // Bound work even if the concurrent fetch worker keeps replenishing.
        for _ in 0..self.capacity {
            let Ok(mut observation) = self.rx.try_recv() else {
                break;
            };
            if !self.guard.active.load(Ordering::Acquire) {
                producer.mark_degraded();
                return;
            }
            if observation.key.source_id != self.source
                || observation.key.epoch != self.fetch_generation
            {
                producer.mark_degraded();
                continue;
            }
            // Explicit rebind after the unique receiver capability and original
            // identity have been validated; never trust a fabricated epoch zero.
            observation.key.epoch = service_epoch;
            if producer.try_observe(observation) != SubmitOutcome::Queued {
                self.guard.signal_loss();
                producer.mark_degraded();
            }
        }
        if !self.guard.active.load(Ordering::Acquire) || self.guard.loss.load(Ordering::Acquire) {
            producer.mark_degraded();
        }
    }
}
impl Drop for HlsBridgeReceiver {
    fn drop(&mut self) {
        self.guard.active.store(false, Ordering::Release);
    }
}
