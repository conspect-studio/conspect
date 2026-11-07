//! Pipeline-owned source-event integration. Control/ingest allocate; readers never lock.
use crate::source_event_ingest::{ConnectionLease, ContextReader, IngestController, SourceLease};
use crate::source_event_service::{
    ServiceLimits, SourceEventProducer, SourceEventRegistry, SourceEventService,
};
use multiview_control::source_events::{
    ActiveBreak, BreakStatus, SourceBreakProvider, SourceEventAvailability, SourceEventDto,
    SourceEventLabel, SourceEventLifecycle, SourceEventOrigin, SourceEventProvider,
    SourceEventTestInjector, SourceEventTimer, SourceEventTimerKind, SourceEventsDto,
    TestEventBoundary, TestEventOutcome,
};
use multiview_core::{
    event_overlay::{
        project_event_overlay, EventOverlay, EventOverlayBinding, EventOverlayPolicy,
        OverlayObservationTime,
    },
    source_event::PresentationClock,
};
use multiview_framestore::{
    tile::{EventClockContext, SourceFrameTime},
    SwapCell,
};
use multiview_input::{
    normalize::PtsMappingSnapshot,
    source_events::{AdapterLimits, ScteSourceEventAdapter},
};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

/// Owns the worker and bounded registration authority for one pipeline.
pub struct Runtime {
    service: SourceEventService,
    ingest: IngestController,
    publication: Mutex<()>,
    reader: RuntimeReader,
}
/// Cloneable lock-free presentation and API reader.
#[derive(Clone)]
pub struct RuntimeReader {
    registry: SourceEventRegistry,
    contexts: Arc<SwapCell<Arc<HashMap<String, ContextReader>>>>,
    sampled: Arc<SwapCell<Arc<HashMap<String, Sample>>>>,
}
#[derive(Clone)]
struct Sample {
    frame: SourceFrameTime,
    epoch: u64,
    evidence: crate::source_event_service::SourceEventRead,
    sampled_at: std::time::Instant,
    presented: bool,
}
impl Runtime {
    /// Start an isolated bounded service.
    /// # Errors
    /// Returns the worker thread creation error.
    pub fn new() -> io::Result<Self> {
        let limits = ServiceLimits::default();
        let service = SourceEventService::new(limits)?;
        let ingest = IngestController::new(service.registration_controller(), limits.max_sources);
        let reader = RuntimeReader {
            registry: service.registry(),
            contexts: Arc::new(SwapCell::new(Arc::new(HashMap::new()))),
            sampled: Arc::new(SwapCell::new(Arc::new(HashMap::new()))),
        };
        Ok(Self {
            service,
            ingest,
            publication: Mutex::new(()),
            reader,
        })
    }
    /// Activate a strictly newer lifecycle off-clock and publish its bounded reader.
    pub fn activate(&self, id: &str, lifecycle: u64) -> Option<SourceLease> {
        let _guard = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lease = self.ingest.activate(id, lifecycle).ok()?;
        let mut contexts = (*self.reader.contexts.load()).clone();
        contexts.insert(id.to_owned(), lease.context_reader());
        self.reader.contexts.store(Arc::new(contexts));
        Some(lease)
    }
    /// Share without transferring worker ownership.
    #[must_use]
    pub fn reader(&self) -> RuntimeReader {
        self.reader.clone()
    }
    /// Join the worker on a supervisor thread, never an output-clock thread.
    /// # Errors
    /// Returns a worker panic payload.
    pub fn shutdown(self) -> std::thread::Result<()> {
        self.service.shutdown()
    }
}
impl RuntimeReader {
    /// Immutable service registry handle.
    #[must_use]
    pub fn registry(&self) -> SourceEventRegistry {
        self.registry.clone()
    }
    /// Source-scoped identity matcher, without a registration lock.
    #[must_use]
    pub fn context(&self, id: &str) -> Option<ContextReader> {
        self.contexts.load().get(id).cloned()
    }
    /// Project only the selected source frame, never an ingest anchor. One caller
    /// owns sampling; clones are intended for API reads. Clearing uncertainty and
    /// terminal states immediately avoids inventing a lifecycle hold clock.
    #[must_use]
    pub fn sample(
        &self,
        selected: &HashMap<String, SourceFrameTime>,
    ) -> HashMap<String, EventOverlay> {
        let contexts = self.contexts.load();
        // Retain bounded historical projections, but never call an omitted source fresh.
        let mut sampled = (*self.sampled.load()).clone();
        sampled.retain(|id, _| contexts.contains_key(id));
        for sample in sampled.values_mut() {
            sample.presented = false;
        }
        let mut overlays = HashMap::new();
        // Iterate the bounded context map, not an untrusted selection map.
        for (id, context) in contexts.iter() {
            let Some(&frame) = selected.get(id) else {
                continue;
            };
            let Some(read) = self.registry.snapshot(id) else {
                continue;
            };
            if read.degraded || !context.matches(&read, frame) || !self.registry.is_current(&read) {
                continue;
            }
            let clock = PresentationClock::Fresh {
                source_id: id.clone(),
                epoch: read.epoch,
                position_ns: frame.pts.as_nanos(),
            };
            let binding = EventOverlayBinding {
                source_id: id.clone(),
                event_id: None,
                origin: None,
                policy: EventOverlayPolicy {
                    warning_lead_ns: u64::MAX,
                    terminal_hold_ns: 0,
                    uncertain_hold_ns: 0,
                },
            };
            // Rank borrowed events with scalar keys, then allocate one visual row.
            let overlay = read
                .events
                .iter()
                .filter_map(|event| event_priority(event, &clock).map(|priority| (priority, event)))
                .min_by(|(a, ea), (b, eb)| a.cmp(b).then_with(|| ea.key.cmp(&eb.key)))
                .and_then(|(_, event)| {
                    project_event_overlay(
                        event,
                        &clock,
                        &binding,
                        OverlayObservationTime {
                            now_ns: 0,
                            lifecycle_since_ns: 0,
                        },
                    )
                });
            if !context.matches(&read, frame) || !self.registry.is_current(&read) {
                continue;
            }
            if let Some(value) = &overlay {
                overlays.insert(id.clone(), value.clone());
            }
            sampled.insert(
                id.clone(),
                Sample {
                    frame,
                    epoch: read.epoch,
                    evidence: read,
                    sampled_at: std::time::Instant::now(),
                    presented: true,
                },
            );
        }
        self.sampled.store(Arc::new(sampled));
        overlays
    }
}
impl SourceEventProvider for RuntimeReader {
    fn source_events(&self, id: &str) -> Option<SourceEventsDto> {
        let context = self.context(id)?;
        let read = self.registry.snapshot(id);
        let samples = self.sampled.load();
        let sample = samples.get(id);
        let valid = read.as_ref().zip(sample).is_some_and(|(read, sample)| {
            sample.presented
                && sample.sampled_at.elapsed() < std::time::Duration::from_secs(1)
                && !read.degraded
                && read.epoch == sample.epoch
                && context.matches(read, sample.frame)
                && self.registry.is_current(read)
        });
        let epoch = read.as_ref().map_or(0, |read| read.epoch);
        // DTOs and historical projections are request-plane work only. The
        // sampled Arc freezes the evidence that belonged to this exact frame.
        let rows = sample
            .filter(|sample| {
                read.as_ref().is_some_and(|read| {
                    read.epoch == sample.epoch
                        && context.matches(read, sample.frame)
                        && self.registry.same_generation(read)
                })
            })
            .map_or_else(Vec::new, |sample| {
                let captured_clock = PresentationClock::Fresh {
                    source_id: id.to_owned(),
                    epoch: sample.epoch,
                    position_ns: sample.frame.pts.as_nanos(),
                };
                sample
                    .evidence
                    .events
                    .iter()
                    .map(|event| {
                        let captured = event_dto(event, &captured_clock);
                        if valid {
                            return captured;
                        }
                        let mut row = event_dto(event, &PresentationClock::Unknown);
                        if matches!(
                            captured.lifecycle,
                            SourceEventLifecycle::Ended
                                | SourceEventLifecycle::Cancelled
                                | SourceEventLifecycle::Superseded
                        ) {
                            row.lifecycle = captured.lifecycle;
                            row.label = captured.label;
                        }
                        row.timer = None;
                        row
                    })
                    .collect()
            });
        Some(SourceEventsDto::new(
            id,
            epoch,
            if valid {
                SourceEventAvailability::Available
            } else {
                SourceEventAvailability::Unavailable
            },
            read.as_ref().is_some_and(|read| read.degraded),
            rows,
        ))
    }
}
impl SourceBreakProvider for RuntimeReader {
    /// Fresh acquisition every call (S1). Only explicit, non-vendor evidence
    /// counts: a timed start must be presented on the latched frame clock; an
    /// untimed start counts only for external/operator evidence. Cancelled,
    /// ended, superseded or uncertain occurrences are never "in break".
    fn break_status(&self, id: &str) -> Option<BreakStatus> {
        use multiview_core::source_event::{EventBoundary, EventLifecycle, EventOrigin};
        let context = self.context(id)?;
        let read = self.registry.snapshot(id)?;
        let samples = self.sampled.load();
        let position = samples.get(id).and_then(|sample| {
            (sample.presented
                && sample.epoch == read.epoch
                && sample.sampled_at.elapsed() < std::time::Duration::from_secs(1)
                && context.matches(&read, sample.frame))
            .then(|| sample.frame.pts.as_nanos())
        });
        let clock = position.map_or(PresentationClock::Unknown, |position_ns| {
            PresentationClock::Fresh {
                source_id: id.to_owned(),
                epoch: read.epoch,
                position_ns,
            }
        });
        let active = read.events.iter().find_map(|event| {
            if matches!(event.key.origin, EventOrigin::VendorMarker { .. })
                || event.key.epoch != read.epoch
            {
                return None;
            }
            let mut start = None;
            let mut ended = false;
            for o in event.observations() {
                match o.boundary {
                    EventBoundary::Start { pts_ns, .. } => start = Some(pts_ns),
                    EventBoundary::End { .. } | EventBoundary::Cancel => ended = true,
                    _ => {}
                }
            }
            let in_break = match start? {
                Some(_) => event.project(&clock).lifecycle == EventLifecycle::Active,
                None => matches!(event.key.origin, EventOrigin::External) && !ended,
            };
            in_break.then(|| ActiveBreak {
                event_id: event.key.event_id.clone(),
                test: event.key.event_id.starts_with("test:"),
            })
        });
        // Anything published between acquisition and here makes this answer
        // obsolete: report degraded so no NEW action starts on it this tick.
        let degraded = read.degraded || !self.registry.is_current(&read);
        Some(BreakStatus {
            epoch: read.epoch,
            active,
            degraded,
        })
    }
}
impl SourceEventTestInjector for RuntimeReader {
    fn inject_test_event(&self, id: &str, boundary: TestEventBoundary) -> TestEventOutcome {
        let boundary = match boundary {
            TestEventBoundary::Start { duration_ns } => {
                crate::source_event_test_inject::TestBoundary::Start { duration_ns }
            }
            TestEventBoundary::End => crate::source_event_test_inject::TestBoundary::End,
        };
        match self.registry.inject_test_observation(id, boundary) {
            None => TestEventOutcome::Unknown,
            Some(
                crate::source_event_service::SubmitOutcome::Queued
                | crate::source_event_service::SubmitOutcome::Invalid,
            ) => TestEventOutcome::Queued,
            Some(_) => TestEventOutcome::Overflow,
        }
    }
}
fn event_priority(
    event: &multiview_core::source_event::SourceEvent,
    clock: &PresentationClock,
) -> Option<(u8, u64)> {
    use multiview_core::source_event::{EventBoundary, EventLifecycle};
    match event.project(clock).lifecycle {
        EventLifecycle::Active => Some((0, 0)),
        EventLifecycle::Announced => {
            let PresentationClock::Fresh { position_ns, .. } = clock else {
                return None;
            };
            event.observations().find_map(|o| match o.boundary {
                EventBoundary::Start {
                    pts_ns: Some(start),
                    ..
                } => Some((1, start.abs_diff(*position_ns))),
                _ => None,
            })
        }
        EventLifecycle::Observed
            if !event
                .observations()
                .any(|o| matches!(o.boundary, EventBoundary::MarkerAbsent)) =>
        {
            Some((2, 0))
        }
        _ => None,
    }
}

#[cfg(test)]
fn overlay_priority(overlay: &EventOverlay) -> (u8, u64, String, String) {
    use multiview_core::source_event::EventLifecycle;
    let priority = match overlay.lifecycle {
        EventLifecycle::Active => 0,
        EventLifecycle::Announced => 1,
        _ => 2,
    };
    let distance = if priority == 1 {
        overlay
            .timer
            .as_ref()
            .and_then(|t| t.nanoseconds.parse().ok())
            .unwrap_or(u64::MAX)
    } else {
        0
    };
    (
        priority,
        distance,
        overlay.key.event_id.clone(),
        format!("{:?}", overlay.key.origin),
    )
}

fn event_dto(
    event: &multiview_core::source_event::SourceEvent,
    clock: &PresentationClock,
) -> SourceEventDto {
    use multiview_core::event_overlay::OverlayLabel;
    let projection = event.project(clock);
    let binding = EventOverlayBinding {
        source_id: event.key.source_id.clone(),
        event_id: None,
        origin: None,
        policy: EventOverlayPolicy {
            warning_lead_ns: u64::MAX,
            terminal_hold_ns: u64::MAX,
            uncertain_hold_ns: u64::MAX,
        },
    };
    let overlay = project_event_overlay(
        event,
        clock,
        &binding,
        OverlayObservationTime {
            now_ns: 0,
            lifecycle_since_ns: 0,
        },
    );
    let fallback = EventOverlay {
        key: (&event.key).into(),
        lifecycle: projection.lifecycle,
        label: OverlayLabel::Uncertain,
        timer: None,
        descriptor: event.descriptor().cloned(),
    };
    let mut row = dto(overlay.as_ref().unwrap_or(&fallback));
    if overlay.is_none() {
        row.label = None;
    }
    // Revisions are independent across boundaries; use documented boundary order,
    // preferring start metadata rather than comparing unrelated revision numbers.
    row.asset_id = event.observations().find_map(|o| o.asset_id.clone());
    row.correlation_id = event.observations().find_map(|o| o.correlation_id.clone());
    row
}

fn dto(overlay: &EventOverlay) -> SourceEventDto {
    use multiview_core::{
        event_overlay::{OverlayLabel as L, OverlayTimerKind as T},
        source_event::{EventLifecycle as E, EventOrigin as O},
    };
    SourceEventDto {
        event_id: overlay.key.event_id.clone(),
        epoch: overlay.key.epoch.clone(),
        origin: match overlay.key.origin {
            O::Scte35 => SourceEventOrigin::Scte35,
            O::TimedId3 => SourceEventOrigin::TimedId3,
            O::Manifest => SourceEventOrigin::Manifest,
            O::VendorMarker { .. } => SourceEventOrigin::VendorMarker,
            O::External => SourceEventOrigin::External,
        },
        vendor: match &overlay.key.origin {
            O::VendorMarker { vendor } => Some(vendor.clone()),
            _ => None,
        },
        lifecycle: match overlay.lifecycle {
            E::Announced => SourceEventLifecycle::Announced,
            E::Active => SourceEventLifecycle::Active,
            E::Ended => SourceEventLifecycle::Ended,
            E::Cancelled => SourceEventLifecycle::Cancelled,
            E::Superseded => SourceEventLifecycle::Superseded,
            E::Uncertain => SourceEventLifecycle::Uncertain,
            E::Observed => SourceEventLifecycle::Observed,
        },
        timer: overlay.timer.as_ref().map(|t| SourceEventTimer {
            kind: match t.kind {
                T::UntilStart => SourceEventTimerKind::UntilStart,
                T::Remaining => SourceEventTimerKind::Remaining,
                T::Elapsed => SourceEventTimerKind::Elapsed,
                T::Overrun => SourceEventTimerKind::Overrun,
            },
            nanoseconds: t.nanoseconds.clone(),
        }),
        label: Some(match overlay.label {
            L::Upcoming => SourceEventLabel::Upcoming,
            L::Active => SourceEventLabel::Active,
            L::Ended => SourceEventLabel::Ended,
            L::Uncertain => SourceEventLabel::Uncertain,
            L::Overrun => SourceEventLabel::Overrun,
            L::Observed => SourceEventLabel::Observed,
        }),
        asset_id: None,
        correlation_id: None,
        marker_name: overlay.descriptor.as_ref().map(|d| d.name.clone()),
        marker_type_id: overlay.descriptor.as_ref().and_then(|d| d.type_id.clone()),
        marker_detail: overlay.descriptor.as_ref().and_then(|d| d.detail.clone()),
    }
}

/// Per-native-connection SCTE adapter, never shared across reconnects.
pub struct NativeEvents {
    source: String,
    connection: ConnectionLease,
    context: Option<EventClockContext>,
    producer: Option<SourceEventProducer>,
    adapter: Option<ScteSourceEventAdapter>,
    preanchor: Vec<Vec<u8>>,
    anchored: bool,
    loss: std::cell::Cell<bool>,
    hls: Option<crate::source_event_hls_bridge::HlsBridgeReceiver>,
    hls_attached: bool,
    service_epoch: Option<u64>,
    id3: Vec<multiview_input::id3::Id3Tag>,
    id3_bytes: usize,
    id3_published: usize,
    untimed_scte: Vec<Vec<multiview_input::scte::splice35::NormalizedCueEvent>>,
    untimed_scte_published: usize,
}
impl NativeEvents {
    /// Allocate a fresh connection capability off-clock.
    #[must_use]
    pub fn new(source: String, lease: &SourceLease) -> Option<Self> {
        Some(Self {
            source,
            connection: lease.connect().ok()?,
            context: None,
            producer: None,
            adapter: None,
            preanchor: Vec::new(),
            anchored: false,
            loss: std::cell::Cell::new(false),
            hls: None,
            hls_attached: false,
            service_epoch: None,
            id3: Vec::new(),
            id3_bytes: 0,
            id3_published: 0,
            untimed_scte: Vec::new(),
            untimed_scte_published: 0,
        })
    }
    /// Attach the unique same-connection HLS receiver before the first mapping.
    /// Rejected receivers are consumed and dropped; attachment is never replaceable.
    pub fn attach_hls(
        &mut self,
        receiver: crate::source_event_hls_bridge::HlsBridgeReceiver,
    ) -> bool {
        if self.anchored || self.hls_attached {
            return false;
        }
        self.hls = Some(receiver);
        self.hls_attached = true;
        true
    }
    /// Drain bounded vendor observations only after registering this connection's mapping.
    pub fn ingest_hls(&mut self) {
        if let (Some(receiver), Some(epoch), Some(producer)) =
            (&mut self.hls, self.service_epoch, &self.producer)
        {
            receiver.drain_into(&self.source, epoch, producer);
        }
    }
    /// Validate ID3 and retain metadata presence only: never a timed break or timer.
    /// At most 16 distinct tags / 256 KiB of input are retained; repeats deduplicate.
    pub fn observe_id3(&mut self, bytes: &[u8]) {
        let Ok(tag) = multiview_input::id3::parse(bytes) else {
            self.mark_degraded();
            return;
        };
        if self.id3.contains(&tag) {
            return;
        }
        if (self.anchored && self.producer.is_none())
            || self.id3.len() >= 16
            || bytes.len() > 262_144_usize.saturating_sub(self.id3_bytes)
        {
            self.mark_degraded();
            return;
        }
        self.id3_bytes += bytes.len();
        self.id3.push(tag);
        self.publish_id3();
    }
    fn publish_id3(&mut self) {
        use multiview_core::source_event::{
            EventBoundary, EventOrigin, SourceEventKey, SourceEventObservation,
        };
        let (Some(producer), Some(epoch)) = (&self.producer, self.service_epoch) else {
            return;
        };
        while self.id3_published < self.id3.len() {
            let descriptor = self
                .id3
                .get(self.id3_published)
                .and_then(multiview_input::id3::marker_descriptor);
            let observation = SourceEventObservation {
                key: SourceEventKey {
                    source_id: self.source.clone(),
                    epoch,
                    origin: EventOrigin::TimedId3,
                    event_id: format!("id3:{}", self.id3_published),
                },
                revision: 1,
                boundary: EventBoundary::MarkerObserved,
                asset_id: None,
                correlation_id: None,
                descriptor,
            };
            let _ = producer.try_observe(observation);
            self.id3_published += 1;
        }
    }
    /// Retain valid SCTE evidence lacking packet/timeline provenance as presence only.
    /// No later anchor can promote this evidence to a start/end or action boundary.
    /// Semantic dedup retains at most 64 section projections (each input <=4096 bytes).
    pub fn observe_scte_untimed(&mut self, bytes: &[u8]) {
        if bytes.len() > 4096 {
            self.mark_degraded();
            return;
        }
        let Ok(section) = multiview_input::scte::SpliceInfoSection::parse(bytes) else {
            self.mark_degraded();
            return;
        };
        let cues = section.normalized_cue_events();
        if cues.is_empty() || self.untimed_scte.contains(&cues) {
            return;
        }
        if (self.anchored && self.producer.is_none()) || self.untimed_scte.len() >= 64 {
            self.mark_degraded();
            return;
        }
        self.untimed_scte.push(cues);
        self.publish_untimed_scte();
    }
    fn publish_untimed_scte(&mut self) {
        use multiview_core::source_event::{
            EventBoundary, EventOrigin, SourceEventKey, SourceEventObservation,
        };
        let (Some(producer), Some(epoch)) = (&self.producer, self.service_epoch) else {
            return;
        };
        while self.untimed_scte_published < self.untimed_scte.len() {
            // Section occurrence namespace is independent of trusted insert/seg IDs.
            // One evidence row per validated section, never an invented break instance.
            let descriptor = self
                .untimed_scte
                .get(self.untimed_scte_published)
                .and_then(|cues| cues.first())
                .and_then(|cue| {
                    cue.segmentation.as_ref().and_then(|d| {
                        let type_id = d.segmentation_type_id?;
                        let name = multiview_input::scte::segmentation_type_name(type_id)?;
                        Some(multiview_core::source_event::MarkerDescriptor {
                            name: name.to_owned(),
                            type_id: Some(format!("0x{type_id:02X}")),
                            detail: None,
                        })
                    })
                });
            let observation = SourceEventObservation {
                key: SourceEventKey {
                    source_id: self.source.clone(),
                    epoch,
                    origin: EventOrigin::Scte35,
                    event_id: format!("untimed-scte:{}", self.untimed_scte_published),
                },
                revision: 1,
                boundary: EventBoundary::MarkerObserved,
                asset_id: None,
                correlation_id: None,
                descriptor,
            };
            let _ = producer.try_observe(observation);
            self.untimed_scte_published += 1;
        }
    }
    fn discard_pending_metadata(&mut self) {
        self.preanchor.clear();
        self.hls = None;
        self.id3.clear();
        self.id3_bytes = 0;
        self.id3_published = 0;
        self.untimed_scte.clear();
        self.untimed_scte_published = 0;
        self.service_epoch = None;
    }
    /// Register generation changes and resolve pending evidence using the same mapping.
    /// Before the first real anchor, retain at most 64 sections of at most 4096 bytes.
    /// A missing mapping after an anchor invalidates pending evidence and latches loss.
    /// `None` carries no generation: callers MUST recreate this connection adapter on
    /// a discontinuity before its first anchor. Reset-gap cues are never remapped.
    pub fn mapping(&mut self, snapshot: Option<PtsMappingSnapshot>) -> Option<EventClockContext> {
        let Some(snapshot) = snapshot else {
            if self.anchored {
                self.discard_pending_metadata();
                self.mark_degraded();
            }
            self.producer = None;
            self.adapter = None;
            self.context = None;
            return None;
        };
        if self
            .context
            .is_none_or(|context| context.mapping_generation != snapshot.generation())
        {
            self.producer = None;
            self.adapter = None;
            self.context = None;
            if self.anchored {
                self.discard_pending_metadata();
                self.loss.set(true);
            }
            self.anchored = true;
            let (context, producer) = self.connection.register(snapshot.generation()).ok()?;
            if self.loss.get() {
                producer.mark_degraded();
            }
            self.adapter = Some(ScteSourceEventAdapter::new(
                self.source.clone(),
                context.service_epoch,
                AdapterLimits::default(),
            ));
            self.service_epoch = Some(context.service_epoch);
            self.producer = Some(producer);
            self.context = Some(context.clock);
        }
        for bytes in std::mem::take(&mut self.preanchor) {
            self.publish_section(&bytes, Some(snapshot));
        }
        if let (Some(adapter), Some(producer)) = (&mut self.adapter, &self.producer) {
            for observation in adapter.resolve_pending(snapshot) {
                let _ = producer.try_observe(observation);
            }
        }
        self.publish_id3();
        self.publish_untimed_scte();
        self.context
    }
    /// Latch upstream evidence loss for the current generation.
    pub fn mark_degraded(&self) {
        self.loss.set(true);
        if let Some(producer) = &self.producer {
            producer.mark_degraded();
        }
    }
    /// Parse one bounded SCTE section; malformed or exhausted evidence fails closed.
    pub fn ingest(&mut self, bytes: &[u8], snapshot: Option<PtsMappingSnapshot>) {
        if self.mapping(snapshot).is_none() {
            if !self.anchored && bytes.len() <= 4096 && self.preanchor.len() < 64 {
                self.preanchor.push(bytes.to_vec());
            } else {
                self.mark_degraded();
            }
            return;
        }
        self.publish_section(bytes, snapshot);
    }
    fn publish_section(&mut self, bytes: &[u8], snapshot: Option<PtsMappingSnapshot>) {
        if let (Some(adapter), Some(producer)) = (&mut self.adapter, &self.producer) {
            match adapter.ingest_section(bytes, 0, snapshot) {
                Ok(observations) => {
                    for observation in observations {
                        let _ = producer.try_observe(observation);
                    }
                }
                Err(_) => producer.mark_degraded(),
            }
            let diagnostics = adapter.diagnostics();
            if diagnostics.capacity_exceeded > 0
                || diagnostics.invalid_identity > 0
                || diagnostics.revision_exhausted > 0
            {
                producer.mark_degraded();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_core::{
        event_overlay::OverlayTimerKind,
        time::{MediaTime, Rational},
    };
    use multiview_input::{
        mpegts::crc::crc32_mpeg2,
        normalize::{PtsNormalizer, WrapBits},
    };

    #[test]
    fn native_hls_attachment_is_unique_and_reset_discards_receiver() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let (_observer, receiver) = crate::source_event_hls_bridge::channel("s".into(), 3, 4);
        assert!(native.attach_hls(receiver));
        let (_other, receiver) = crate::source_event_hls_bridge::channel("s".into(), 3, 4);
        assert!(!native.attach_hls(receiver));
        native.ingest_hls();
        assert!(native.hls.is_some());
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        n.normalize(Some(0), 0).expect("anchor");
        native.mapping(n.mapping_snapshot()).expect("clock");
        native.ingest_hls();
        native.mapping(None);
        assert!(native.hls.is_none());
        runtime.shutdown().expect("join");
    }
    #[test]
    fn native_id3_is_deduplicated_presence_without_timing() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let tag = b"ID3\x04\x00\x00\x00\x00\x00\x0cTXXX\x00\x00\x00\x02\x00\x00\x03x";
        native.observe_id3(tag);
        native.observe_id3(tag);
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        n.normalize(Some(0), 0).expect("anchor");
        native.mapping(n.mapping_snapshot()).expect("clock");
        wait_evidence(&reader);
        let read = reader.registry.snapshot("s").expect("snapshot");
        assert_eq!(read.events.len(), 1);
        for o in read
            .events
            .iter()
            .flat_map(multiview_core::source_event::SourceEvent::observations)
        {
            assert_eq!(
                o.key.origin,
                multiview_core::source_event::EventOrigin::TimedId3
            );
            assert_eq!(
                o.boundary,
                multiview_core::source_event::EventBoundary::MarkerObserved
            );
        }
        native.mapping(None);
        assert_eq!(native.id3, Vec::<multiview_input::id3::Id3Tag>::new());
        runtime.shutdown().expect("join");
    }
    #[test]
    fn native_untimed_scte_stays_presence_after_anchor_and_deduplicates() {
        use crate::source_event_packet::{MetadataKind, MetadataPacket, PacketGate};
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let mut gate = PacketGate::default();
        assert!(gate
            .push(MetadataPacket {
                kind: MetadataKind::Scte35,
                bytes: cue(),
                pts: None,
                time_base_num: 1,
                time_base_den: 90_000,
                video_fence: 0
            })
            .is_none());
        let packets = gate.decoded(None, None, 1, 90_000);
        assert_eq!(packets.len(), 1);
        for gated in packets {
            assert!(!gated.trusted);
            native.observe_scte_untimed(&gated.packet.bytes);
        }
        native.observe_scte_untimed(&cue());
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        n.normalize(None, 0).expect("genpts");
        native.mapping(n.mapping_snapshot());
        n.normalize(Some(0), 0).expect("anchor");
        native.mapping(n.mapping_snapshot()).expect("clock");
        wait_evidence(&reader);
        let read = reader.registry.snapshot("s").expect("snapshot");
        assert_eq!(read.events.len(), 1);
        for o in read
            .events
            .iter()
            .flat_map(multiview_core::source_event::SourceEvent::observations)
        {
            assert_eq!(
                o.key.origin,
                multiview_core::source_event::EventOrigin::Scte35
            );
            assert!(o.key.event_id.starts_with("untimed-scte:"));
            assert_eq!(
                o.boundary,
                multiview_core::source_event::EventBoundary::MarkerObserved
            );
        }
        assert_eq!(
            native
                .adapter
                .as_ref()
                .expect("adapter")
                .retained_boundaries(),
            0
        );
        let clock = native.mapping(n.mapping_snapshot()).expect("clock");
        let selected = HashMap::from([(
            "s".into(),
            SourceFrameTime {
                pts: MediaTime::ZERO,
                generation: 1,
                event_clock: Some(clock),
            },
        )]);
        let _ = reader.sample(&selected);
        let api = serde_json::to_value(reader.source_events("s").expect("API")).expect("JSON");
        assert_eq!(api["events"].as_array().expect("events").len(), 1);
        assert_eq!(api["events"][0]["lifecycle"], "observed");
        assert!(api["events"][0]["timer"].is_null());
        native.mapping(None);
        assert_eq!(
            native.untimed_scte,
            Vec::<Vec<multiview_input::scte::NormalizedCueEvent>>::new()
        );
        runtime.shutdown().expect("join");
    }
    #[test]
    fn native_untimed_scte_bounds_and_bad_crc_fail_closed() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let bytes = cue();
        for adjustment in 0..65 {
            let section = multiview_input::scte::SpliceInfoSection::parse(&bytes).expect("section");
            let variant = section
                .reserialize_with_pts_adjustment(adjustment)
                .expect("adjustment");
            native.observe_scte_untimed(&variant);
        }
        assert_eq!(native.untimed_scte.len(), 64);
        assert!(native.loss.get());
        let mut other = NativeEvents::new("s".into(), &lease).expect("connection");
        let mut invalid = bytes;
        *invalid.last_mut().expect("CRC") ^= 1;
        other.observe_scte_untimed(&invalid);
        assert_eq!(
            other.untimed_scte,
            Vec::<Vec<multiview_input::scte::NormalizedCueEvent>>::new()
        );
        assert!(other.loss.get());
        runtime.shutdown().expect("join");
    }
    fn cue() -> Vec<u8> {
        // time_signal(90000) with segmentation break-start id 7.
        let mut descriptor = b"CUEI".to_vec();
        descriptor.extend(7_u32.to_be_bytes());
        descriptor.extend([0x7f, 0xbf, 0, 0, 0x22, 1, 1]);
        let mut section = vec![0xfc, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xf0, 5, 6, 0xfe];
        section.extend(90_000_u32.to_be_bytes());
        section.extend(
            u16::try_from(descriptor.len() + 2)
                .expect("length")
                .to_be_bytes(),
        );
        section.extend([2, u8::try_from(descriptor.len()).expect("length")]);
        section.extend(descriptor);
        let length = u16::try_from(section.len() + 1)
            .expect("length")
            .to_be_bytes();
        section[1] = 0x30 | length[0];
        section[2] = length[1];
        section.extend(crc32_mpeg2(&section).to_be_bytes());
        section
    }
    fn wait_evidence(reader: &RuntimeReader) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while reader
            .registry
            .snapshot("s")
            .is_none_or(|read| read.events.is_empty())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "worker failed to publish cue"
            );
            std::thread::yield_now();
        }
    }
    #[test]
    fn runtime_history_priority_assets_and_unavailable_timers() {
        use multiview_core::source_event::{
            EventBoundary, EventOrigin, SourceEventKey, SourceEventObservation,
        };
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let connection = lease.connect().expect("connection");
        let (context, producer) = connection.register(1).expect("registration");
        let reader = runtime.reader();
        for (id, boundary) in [
            (
                "a-future",
                EventBoundary::Start {
                    pts_ns: Some(1000),
                    duration_ns: None,
                },
            ),
            (
                "b-active",
                EventBoundary::Start {
                    pts_ns: Some(0),
                    duration_ns: None,
                },
            ),
            ("c-ended", EventBoundary::End { pts_ns: Some(50) }),
            ("d-cancelled", EventBoundary::Cancel),
        ] {
            let _ = producer.try_observe(SourceEventObservation {
                key: SourceEventKey {
                    source_id: "s".into(),
                    epoch: context.service_epoch,
                    origin: EventOrigin::Scte35,
                    event_id: id.into(),
                },
                revision: 1,
                boundary,
                asset_id: Some("asset".into()),
                correlation_id: Some("correlation".into()),
                descriptor: None,
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while reader
            .registry
            .snapshot("s")
            .is_none_or(|r| r.events.len() != 4)
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let initial =
            serde_json::to_value(reader.source_events("s").expect("known")).expect("json");
        assert_eq!(initial["events"], serde_json::json!([]));
        let selected = HashMap::from([(
            "s".into(),
            SourceFrameTime {
                pts: MediaTime::from_nanos(100),
                generation: 1,
                event_clock: Some(context.clock),
            },
        )]);
        assert_eq!(reader.sample(&selected)["s"].key.event_id, "b-active");
        let evidence = reader.registry.snapshot("s").expect("snapshot").evidence;
        assert!(Arc::ptr_eq(
            &reader.sampled.load()["s"].evidence.evidence,
            &evidence
        ));
        assert_eq!(reader.sample(&selected)["s"].key.event_id, "b-active");
        assert!(Arc::ptr_eq(
            &reader.sampled.load()["s"].evidence.evidence,
            &evidence
        ));
        let api = serde_json::to_value(reader.source_events("s").expect("known")).expect("json");
        assert_eq!(api["events"].as_array().expect("rows").len(), 4);
        assert_eq!(api["events"][2]["lifecycle"], "ended");
        assert_eq!(api["events"][3]["lifecycle"], "cancelled");
        assert_eq!(api["events"][1]["asset_id"], "asset");
        assert_eq!(api["events"][1]["correlation_id"], "correlation");
        assert!(reader.sample(&HashMap::new()).is_empty());
        let stale = serde_json::to_value(reader.source_events("s").expect("known")).expect("json");
        assert_eq!(stale["availability"], "unavailable");
        assert_eq!(stale["events"].as_array().expect("rows").len(), 4);
        assert_eq!(stale["events"][2]["lifecycle"], "ended");
        assert_eq!(stale["events"][3]["lifecycle"], "cancelled");
        assert!(stale["events"]
            .as_array()
            .expect("rows")
            .iter()
            .all(|r| r["timer"].is_null()));
        runtime.shutdown().expect("join");
    }

    #[test]
    fn runtime_nearest_upcoming_is_not_first_identity() {
        use multiview_core::source_event::{
            EventBoundary, EventOrigin, SourceEventKey, SourceEventObservation, SourceEventTracker,
            TrackerLimits,
        };
        let mut tracker = SourceEventTracker::new(TrackerLimits::default());
        for (id, pts) in [("a-far", 1000), ("z-near", 100)] {
            tracker.observe(SourceEventObservation {
                key: SourceEventKey {
                    source_id: "s".into(),
                    epoch: 1,
                    origin: EventOrigin::Scte35,
                    event_id: id.into(),
                },
                revision: 1,
                boundary: EventBoundary::Start {
                    pts_ns: Some(pts),
                    duration_ns: None,
                },
                asset_id: None,
                correlation_id: None,
                descriptor: None,
            });
        }
        let clock = PresentationClock::Fresh {
            source_id: "s".into(),
            epoch: 1,
            position_ns: 0,
        };
        let binding = EventOverlayBinding {
            source_id: "s".into(),
            event_id: None,
            origin: None,
            policy: EventOverlayPolicy {
                warning_lead_ns: u64::MAX,
                terminal_hold_ns: 0,
                uncertain_hold_ns: 0,
            },
        };
        let selected = tracker
            .events()
            .filter_map(|e| {
                project_event_overlay(
                    e,
                    &clock,
                    &binding,
                    OverlayObservationTime {
                        now_ns: 0,
                        lifecycle_since_ns: 0,
                    },
                )
            })
            .min_by_key(overlay_priority)
            .expect("overlay");
        assert_eq!(selected.key.event_id, "z-near");
    }

    #[test]
    fn native_cue_selected_pts_and_api_share_projection_and_retire() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 9).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let mut normalizer = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        normalizer.normalize(Some(0), 0).expect("normalize");
        let snapshot = normalizer.mapping_snapshot();
        let clock = native.mapping(snapshot).expect("clock");
        native.ingest(&cue(), snapshot);
        wait_evidence(&reader);
        // Advance the ingest anchor far ahead: output still presents the older frame.
        normalizer
            .normalize(Some(900_000), 10_000_000_000)
            .expect("normalize");
        native.mapping(normalizer.mapping_snapshot());
        let mut selected = HashMap::from([(
            "s".into(),
            SourceFrameTime {
                pts: MediaTime::from_nanos(500_000_000),
                generation: 9,
                event_clock: Some(clock),
            },
        )]);
        let overlays = reader.sample(&selected);
        let overlay = overlays.get("s").expect("overlay");
        let timer = overlay.timer.as_ref().expect("countdown");
        assert_eq!(timer.kind, OverlayTimerKind::UntilStart);
        assert_eq!(timer.nanoseconds, "500000000");
        let api = serde_json::to_value(reader.source_events("s").expect("known")).expect("json");
        assert_eq!(api["events"][0]["timer"]["nanoseconds"], "500000000");
        selected.get_mut("s").expect("frame").pts = MediaTime::from_nanos(2_000_000_000);
        let overlays = reader.sample(&selected);
        assert_eq!(
            overlays["s"].timer.as_ref().expect("elapsed").kind,
            OverlayTimerKind::Elapsed
        );
        // Identity mismatch clears rather than using a latest anchor.
        selected.get_mut("s").expect("frame").generation = 8;
        assert!(reader.sample(&selected).is_empty());
        selected.get_mut("s").expect("frame").generation = 9;
        assert!(!reader.sample(&selected).is_empty());
        lease.retire();
        let api = serde_json::to_value(reader.source_events("s").expect("known")).expect("json");
        assert_eq!(api["availability"], "unavailable");
        assert_eq!(api["events"], serde_json::json!([]));
        assert!(reader.sample(&selected).is_empty());
        runtime.shutdown().expect("join");
    }
    #[test]
    fn native_preanchor_cue_survives_genpts_and_maps_original_pts() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        native.ingest(&cue(), None);
        n.normalize(None, 0).expect("genpts");
        assert!(native.mapping(n.mapping_snapshot()).is_none());
        n.normalize(Some(0), 5_000_000_000).expect("anchor");
        let snapshot = n.mapping_snapshot().expect("snapshot");
        native.mapping(Some(snapshot)).expect("clock");
        wait_evidence(&reader);
        let read = reader.registry.snapshot("s").expect("registered");
        let expected = snapshot
            .project(90_000, Rational::new(1, 90_000), WrapBits::Mpeg33)
            .expect("project")
            .as_nanos();
        assert!(read.events.iter().flat_map(multiview_core::source_event::SourceEvent::observations).any(|o| matches!(o.boundary, multiview_core::source_event::EventBoundary::Start { pts_ns: Some(p), .. } if p == expected)));
        runtime.shutdown().expect("join");
    }
    #[test]
    fn native_preanchor_overflow_and_loss_latch_degrade_registration() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        for _ in 0..65 {
            native.ingest(&cue(), None);
        }
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        n.normalize(Some(0), 0).expect("anchor");
        native.mapping(n.mapping_snapshot()).expect("clock");
        assert!(reader.registry.snapshot("s").expect("registered").degraded);
        runtime.shutdown().expect("join");
    }
    #[test]
    fn native_mapped_reset_drops_gap_cues_instead_of_replaying() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        n.normalize(Some(0), 0).expect("anchor");
        native.mapping(n.mapping_snapshot()).expect("clock");
        n.mark_discontinuity();
        native.ingest(&cue(), n.mapping_snapshot());
        n.normalize(Some(0), 9_000_000_000).expect("anchor");
        native.mapping(n.mapping_snapshot()).expect("new clock");
        assert!(reader
            .registry
            .snapshot("s")
            .expect("registered")
            .events
            .is_empty());
        runtime.shutdown().expect("join");
    }
    #[test]
    fn loss_missing_mapping_and_reconnect_fail_closed() {
        let runtime = Runtime::new().expect("worker");
        let lease = runtime.activate("s", 1).expect("lease");
        let reader = runtime.reader();
        let mut native = NativeEvents::new("s".into(), &lease).expect("connection");
        let mut n = PtsNormalizer::new(
            WrapBits::Mpeg33,
            Rational::new(1, 90_000),
            Rational::new(25, 1),
        );
        n.normalize(Some(0), 0).expect("normalize");
        let clock = native.mapping(n.mapping_snapshot()).expect("clock");
        native.ingest(&cue(), n.mapping_snapshot());
        wait_evidence(&reader);
        let selected = HashMap::from([(
            "s".into(),
            SourceFrameTime {
                pts: MediaTime::ZERO,
                generation: 1,
                event_clock: Some(clock),
            },
        )]);
        assert!(!reader.sample(&selected).is_empty());
        native.mark_degraded();
        assert!(reader.sample(&selected).is_empty());
        assert!(native.mapping(None).is_none());
        assert!(reader.registry.snapshot("s").is_none());
        let mut replacement = NativeEvents::new("s".into(), &lease).expect("connection");
        let newer = replacement.mapping(n.mapping_snapshot()).expect("clock");
        assert_ne!(newer.connection_epoch, clock.connection_epoch);
        assert!(native.mapping(n.mapping_snapshot()).is_none());
        assert!(reader.sample(&selected).is_empty());
        assert!(reader.source_events("unknown").is_none());
        runtime.shutdown().expect("join");
    }
}
