//! Bounded HLS presence evidence. No receipt clock, splice inference or global registry.
use crate::source_event_service::{SourceEventProducer, SubmitOutcome};
use multiview_core::source_event::{
    EventBoundary, EventOrigin, MarkerDescriptor, SourceEventKey, SourceEventObservation,
};
use multiview_input::hls::{CueMetadata, PlaylistCueMetadata, PlaylistCueObservation};
use std::collections::BTreeMap;

/// Nonblocking evidence destination. Implementations must never wait for a consumer.
pub trait HlsEvidenceSink: Send {
    /// Submit one observation; false means evidence was lost.
    fn try_observe(&mut self, observation: SourceEventObservation) -> bool;
    /// Latch incomplete evidence until the source epoch is replaced.
    fn mark_degraded(&mut self);
    /// Retire this generation and invalidate its previously submitted evidence.
    /// Called only off-clock, after any in-flight observation batch completes.
    fn retire(&mut self);
}
impl HlsEvidenceSink for SourceEventProducer {
    fn try_observe(&mut self, observation: SourceEventObservation) -> bool {
        SourceEventProducer::try_observe(self, observation) == SubmitOutcome::Queued
    }
    fn mark_degraded(&mut self) {
        SourceEventProducer::mark_degraded(self);
    }
    fn retire(&mut self) {
        SourceEventProducer::retire(self);
    }
}

/// Explicit per-fetch owner. Move into `HlsFetcher::start_with_observer`.
/// The producer's registered source/epoch must match these constructor arguments.
/// Replace service registration BEFORE beginning a replacement fetch generation.
pub struct HlsEventObserver {
    source: String,
    epoch: u64,
    sink: Box<dyn HlsEvidenceSink>,
    capacity: usize,
    max_string_bytes: usize,
    next: u64,
    // Raw CAID membership -> per-appearance identity and retained display label.
    present: BTreeMap<String, (String, String)>,
    pending: Vec<PlaylistCueObservation>,
    degraded: bool,
}
impl HlsEventObserver {
    /// Construct off-clock with explicit finite membership and payload bounds.
    pub fn new(
        source: String,
        epoch: u64,
        sink: impl HlsEvidenceSink + 'static,
        capacity: usize,
        max_string_bytes: usize,
    ) -> Self {
        Self {
            source,
            epoch,
            sink: Box::new(sink),
            capacity,
            max_string_bytes,
            next: 0,
            present: BTreeMap::new(),
            pending: Vec::new(),
            degraded: false,
        }
    }
    /// Retire the sink generation after the coordinator has stopped submissions.
    pub fn retire(&mut self) {
        self.sink.retire();
    }
    /// Original DATERANGE evidence from the latest complete snapshot, pending
    /// SCTE decoding and an explicit media mapping. Never interpreted as UTC now.
    #[must_use]
    pub fn pending_date_ranges(&self) -> &[PlaylistCueObservation] {
        &self.pending
    }
    /// Whether any evidence was incomplete or lost in this epoch.
    #[must_use]
    pub fn degraded(&self) -> bool {
        self.degraded
    }
    /// Latch loss; subsequent snapshots cannot manufacture withdrawals.
    pub fn mark_degraded(&mut self) {
        self.degraded = true;
        self.sink.mark_degraded();
    }
    /// Observe a complete original playlist, before it is rewritten for loopback.
    /// Invalid snapshots are atomic rejections, not empty presence snapshots.
    pub fn observe(&mut self, snapshot: &PlaylistCueMetadata) {
        if snapshot.rejected != 0
            || snapshot.dropped != 0
            || snapshot.observations.len() > self.capacity
        {
            self.mark_degraded();
            return;
        }
        let mut assets = BTreeMap::new();
        let mut pending = Vec::new();
        for item in &snapshot.observations {
            match &item.metadata {
                CueMetadata::Asset {
                    raw_caid,
                    decoded_label,
                } => {
                    if raw_caid.is_empty()
                        || raw_caid.len() > self.max_string_bytes
                        || decoded_label
                            .as_ref()
                            .is_some_and(|s| s.len() > self.max_string_bytes)
                    {
                        self.mark_degraded();
                        return;
                    }
                    let label = decoded_label
                        .as_deref()
                        .filter(|label| {
                            !label.is_empty() && label.bytes().all(|b| (0x20..=0x7e).contains(&b))
                        })
                        .unwrap_or(raw_caid);
                    assets.insert(raw_caid.clone(), label.to_owned());
                }
                CueMetadata::DateRange { attributes } => {
                    if attributes.len() > 64
                        || attributes.iter().any(|(k, v)| {
                            k.len() > self.max_string_bytes || v.len() > self.max_string_bytes
                        })
                    {
                        self.mark_degraded();
                        return;
                    }
                    pending.push(item.clone());
                }
            }
        }
        if assets
            .keys()
            .filter(|a| !self.present.contains_key(*a))
            .count()
            + self.present.len()
            > self.capacity
            && self.degraded
        {
            self.mark_degraded();
            return;
        }
        self.pending = pending;
        if !self.degraded {
            let absent: Vec<_> = self
                .present
                .keys()
                .filter(|a| !assets.contains_key(*a))
                .cloned()
                .collect();
            for asset in absent {
                if let Some((id, label)) = self.present.get(&asset).cloned() {
                    if !self.emit(&asset, &label, &id, EventBoundary::MarkerAbsent) {
                        return;
                    }
                    self.present.remove(&asset);
                }
            }
        }
        for (asset, label) in &assets {
            if self.present.contains_key(asset) {
                continue;
            }
            if self.present.len() >= self.capacity {
                self.mark_degraded();
                return;
            }
            let Some(next) = self.next.checked_add(1) else {
                self.mark_degraded();
                return;
            };
            self.next = next;
            let id = format!("hls-{}-{}", self.epoch, next);
            if !self.emit(asset, label, &id, EventBoundary::MarkerObserved) {
                return;
            }
            self.present.insert(asset.clone(), (id, label.clone()));
        }
    }
    fn emit(&mut self, asset: &str, label: &str, id: &str, boundary: EventBoundary) -> bool {
        // "Asset <caid>" names reusable asset identity, never a break boundary
        // (CAID presence is not proof the next segment airs an ad).
        let descriptor = matches!(boundary, EventBoundary::MarkerObserved).then(|| {
            let mut name = format!("Asset {label}");
            name.truncate(48);
            MarkerDescriptor {
                name,
                type_id: Some("asset".to_owned()),
                detail: None,
            }
        });
        let accepted = self.sink.try_observe(SourceEventObservation {
            key: SourceEventKey {
                source_id: self.source.clone(),
                epoch: self.epoch,
                origin: EventOrigin::VendorMarker {
                    vendor: "hls-asset".into(),
                },
                event_id: id.into(),
            },
            revision: 0,
            boundary,
            correlation_id: Some(asset.into()),
            asset_id: Some(label.into()),
            descriptor,
        });
        if !accepted {
            self.mark_degraded();
        }
        accepted
    }
}
