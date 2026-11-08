//! Off-clock source-event identity allocation and immutable selected-frame matching.
//! A bounded owner belongs to a pipeline, never a process-global source registry.
use crate::source_event_service::{
    RegisterError, RegistrationController, SourceEventProducer, SourceEventRead,
};
use multiview_framestore::{
    tile::{EventClockContext, SourceFrameTime},
    SwapCell,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// Exact identity associated with one service registration. Service epochs are
/// deliberately not compared to the shared audio/caption lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationContext {
    /// Shared source lifecycle; never advanced for an event mapping change.
    pub lifecycle: u64,
    /// Connection-qualified normalizer generation carried with the frame.
    pub clock: EventClockContext,
    /// Independent monotonic service registration epoch.
    pub service_epoch: u64,
}
/// Lock-free latest context; older buffered frames fail closed after replacement.
#[derive(Clone)]
pub struct ContextReader {
    source: String,
    context: Arc<SwapCell<Option<Arc<RegistrationContext>>>>,
}
impl ContextReader {
    /// Match evidence and the actual selected frame, not a latest PTS anchor.
    /// This is identity validation only: callers must also check registry
    /// `is_current`, degradation and freshness at use time. A retained read is
    /// not an authorization to act after producer retirement or service shutdown.
    #[must_use]
    pub fn matches(&self, read: &SourceEventRead, frame: SourceFrameTime) -> bool {
        self.context.load().is_some_and(|c| {
            self.source == read.evidence.source_id
                && c.service_epoch == read.epoch
                && c.service_epoch == read.evidence.epoch
                && c.lifecycle == frame.generation
                && Some(c.clock) == frame.event_clock
        })
    }
}
struct SourceState {
    lifecycle: u64,
    retired: bool,
    connection: u64,
    service: u64,
    context: Arc<SwapCell<Option<Arc<RegistrationContext>>>>,
}
struct State {
    sources: BTreeMap<String, SourceState>,
    capacity: usize,
}
/// Pipeline-owned allocation authority. Activate only from source control, never
/// from a reconnect loop. Tombstones consume the fixed lifetime source budget.
#[derive(Clone)]
pub struct IngestController {
    registration: RegistrationController,
    state: Arc<Mutex<State>>,
}
/// Capability for exactly one source lifecycle. Old capabilities cannot register
/// a higher service epoch after replacement, even when their reconnect is delayed.
#[derive(Clone)]
pub struct SourceLease {
    owner: IngestController,
    source: String,
    lifecycle: u64,
}
/// Capability for one connection; a delayed old connection cannot remap a new one.
pub struct ConnectionLease {
    lease: SourceLease,
    connection: u64,
}
impl IngestController {
    /// Construct one authority for a service, with a fixed lifetime source budget.
    /// Do not create competing authorities for the same service registration handle.
    #[must_use]
    pub fn new(registration: RegistrationController, capacity: usize) -> Self {
        Self {
            registration,
            state: Arc::new(Mutex::new(State {
                sources: BTreeMap::new(),
                capacity,
            })),
        }
    }
    /// Strictly advance a source lifecycle under the same lock used to register.
    ///
    /// # Errors
    /// Returns an invalid-source, stale-epoch, or capacity error when the source
    /// identifier, lifecycle advance, or lifetime source budget is invalid.
    pub fn activate(&self, source: &str, lifecycle: u64) -> Result<SourceLease, RegisterError> {
        if source.is_empty() || source.len() > 1024 {
            return Err(RegisterError::InvalidSource);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(s) = state.sources.get_mut(source) {
            if lifecycle <= s.lifecycle {
                return Err(RegisterError::StaleEpoch);
            }
            s.lifecycle = lifecycle;
            s.retired = false;
            s.context.store(None);
        } else {
            if state.sources.len() >= state.capacity {
                return Err(RegisterError::Capacity);
            }
            state.sources.insert(
                source.into(),
                SourceState {
                    lifecycle,
                    retired: false,
                    connection: 0,
                    service: 0,
                    context: Arc::new(SwapCell::new(None)),
                },
            );
        }
        Ok(SourceLease {
            owner: self.clone(),
            source: source.into(),
            lifecycle,
        })
    }
}
impl SourceLease {
    /// Obtain a source-scoped lock-free reader on the control/ingest thread.
    pub fn context_reader(&self) -> ContextReader {
        let state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ContextReader {
            source: self.source.clone(),
            context: state.sources.get(&self.source).map_or_else(
                || Arc::new(SwapCell::new(None)),
                |source| Arc::clone(&source.context),
            ),
        }
    }
    /// Allocate once per successful native demux connection, never per packet.
    ///
    /// # Errors
    /// Returns a stale-epoch error for a retired/replaced lease or an exhausted
    /// connection counter. Exhaustion invalidates the previously matched context.
    pub fn connect(&self) -> Result<ConnectionLease, RegisterError> {
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let s = state
            .sources
            .get_mut(&self.source)
            .ok_or(RegisterError::StaleEpoch)?;
        if s.retired || s.lifecycle != self.lifecycle {
            return Err(RegisterError::StaleEpoch);
        }
        // Retire the old match before allocation: exhaustion must fail closed.
        s.context.store(None);
        s.connection = s
            .connection
            .checked_add(1)
            .ok_or(RegisterError::StaleEpoch)?;
        Ok(ConnectionLease {
            lease: self.clone(),
            connection: s.connection,
        })
    }
    /// Retire without advancing the shared audio/caption lifecycle.
    pub fn retire(&self) {
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(s) = state.sources.get_mut(&self.source) {
            if s.lifecycle == self.lifecycle {
                s.context.store(None);
                s.retired = true;
            }
        }
    }
}
impl ConnectionLease {
    /// Register the explicit normalizer generation. No epoch arithmetic aliases
    /// lifecycle, connection or mapping identity. Call only off the output clock.
    ///
    /// # Errors
    /// Returns stale-epoch for obsolete capabilities, non-advancing mappings or
    /// exhausted service counters, and propagates registration errors. Once a
    /// newer mapping is accepted, allocation/registration failure clears the old
    /// context rather than allowing stale frame matches.
    pub fn register(
        &self,
        mapping_generation: u64,
    ) -> Result<(RegistrationContext, SourceEventProducer), RegisterError> {
        let mut state = self
            .lease
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let s = state
            .sources
            .get_mut(&self.lease.source)
            .ok_or(RegisterError::StaleEpoch)?;
        if s.retired || s.lifecycle != self.lease.lifecycle || s.connection != self.connection {
            return Err(RegisterError::StaleEpoch);
        }
        if s.context
            .load()
            .is_some_and(|c| mapping_generation <= c.clock.mapping_generation)
        {
            return Err(RegisterError::StaleEpoch);
        }
        // This mapping supersedes the current context even if allocation or
        // registration fails. Never keep matching evidence from the old mapping.
        s.context.store(None);
        let epoch = s.service.checked_add(1).ok_or(RegisterError::StaleEpoch)?;
        let producer = self
            .lease
            .owner
            .registration
            .register(&self.lease.source, epoch)?;
        s.service = epoch;
        let context = RegistrationContext {
            lifecycle: self.lease.lifecycle,
            clock: EventClockContext {
                connection_epoch: self.connection,
                mapping_generation,
            },
            service_epoch: epoch,
        };
        s.context.store(Some(Arc::new(context)));
        Ok((context, producer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_event_service::{ServiceLimits, SourceEventService};

    #[test]
    fn exhausted_counters_invalidate_populated_context() {
        for connection_exhausted in [true, false] {
            let service = SourceEventService::new(ServiceLimits::default()).expect("worker");
            let owner = IngestController::new(service.registration_controller(), 1);
            let lease = owner.activate("a", 1).expect("activate");
            let reader = lease.context_reader();
            let connection = lease.connect().expect("connect");
            let (context, _producer) = connection.register(0).expect("register");
            let evidence = service.registry().snapshot("a").expect("snapshot");
            let frame = SourceFrameTime {
                pts: multiview_core::time::MediaTime::ZERO,
                generation: context.lifecycle,
                event_clock: Some(context.clock),
            };
            assert!(reader.matches(&evidence, frame));
            {
                let mut state = owner.state.lock().expect("lock");
                let source = state.sources.get_mut("a").expect("source");
                if connection_exhausted {
                    source.connection = u64::MAX;
                } else {
                    source.service = u64::MAX;
                }
            }
            if connection_exhausted {
                assert!(matches!(lease.connect(), Err(RegisterError::StaleEpoch)));
            } else {
                assert!(matches!(
                    connection.register(1),
                    Err(RegisterError::StaleEpoch)
                ));
            }
            assert!(
                !reader.matches(&evidence, frame),
                "exhausted connection: {connection_exhausted}"
            );
            service.shutdown().expect("join");
        }
    }

    #[test]
    fn exhausted_counters_never_wrap_or_publish_context() {
        let service = SourceEventService::new(ServiceLimits::default()).expect("worker");
        let owner = IngestController::new(service.registration_controller(), 1);
        let lease = owner.activate("a", 1).expect("activate");
        let reader = lease.context_reader();
        {
            let mut state = owner.state.lock().expect("lock");
            state.sources.get_mut("a").expect("source").connection = u64::MAX;
        }
        assert!(matches!(lease.connect(), Err(RegisterError::StaleEpoch)));
        {
            let mut state = owner.state.lock().expect("lock");
            let source = state.sources.get_mut("a").expect("source");
            source.connection = 0;
            source.service = u64::MAX;
        }
        let connection = lease.connect().expect("connect");
        assert!(matches!(
            connection.register(0),
            Err(RegisterError::StaleEpoch)
        ));
        assert!(reader.context.load().is_none());
        assert!(service.registry().snapshot("a").is_none());
        service.shutdown().expect("join");
    }
}
