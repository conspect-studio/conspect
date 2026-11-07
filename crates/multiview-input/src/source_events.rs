//! Bounded SCTE-35 to source-event adapter; no pipeline publication or clocks.
//!
//! One instance belongs to one source epoch. Replace it on discontinuity/reconnect;
//! never supply a snapshot from another source/epoch. Callers guarantee cues are
//! within half a 33-bit wrap of the snapshot anchor. Occurrence generations must
//! remain stable across retransmissions AND matching start/end/cancel, including
//! ends received first. Use generation zero when IDs are never reused in an epoch;
//! never derive generations from packet sequence, UPID or receipt time. Reuse needs
//! an external occurrence policy; this adapter deliberately does not guess one.
//! SCTE carries no sender revision counter: revisions here order local observations,
//! not sender intent. Previously seen semantics never rewind state. An unseen late
//! change remains ambiguous; armed rescheduling requires a separate ordering policy.
use crate::{
    normalize::{PtsMappingSnapshot, WrapBits},
    scte::{CueKind, ScteError, SpliceInfoSection},
};
use multiview_core::{
    source_event::{
        EventBoundary, EventOrigin, MarkerDescriptor, SourceEventKey, SourceEventObservation,
    },
    time::Rational,
};
use std::collections::BTreeMap;

/// Finite retention. Full storage rejects new boundaries without evicting dedup keys.
#[derive(Debug, Clone, Copy)]
pub struct AdapterLimits {
    /// Total semantic-history entries across all boundaries, including initial values.
    /// Revisions consume this budget; mapping resolution does not. No eviction.
    pub max_boundaries: usize,
    /// Maximum source identity bytes. Oversize/empty identities disable ingestion.
    pub max_source_bytes: usize,
}
impl Default for AdapterLimits {
    fn default() -> Self {
        Self {
            max_boundaries: 768,
            max_source_bytes: 1024,
        }
    }
}
/// Saturating counters, not an unbounded diagnostic queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AdapterDiagnostics {
    /// Unsupported/generic semantics skipped, never treated as breaks.
    pub unsupported: u64,
    /// New boundary slots rejected at capacity.
    pub capacity_exceeded: u64,
    /// Invalid source identities rejected.
    pub invalid_identity: u64,
    /// Exact semantic duplicates suppressed.
    pub duplicates: u64,
    /// Revisions exhausted (fail closed).
    pub revision_exhausted: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Semantic {
    edge: u8,
    pts: Option<u64>,
    duration: Option<i64>,
    asset: Option<String>,
    descriptor: Option<MarkerDescriptor>,
}
#[derive(Debug)]
struct Slot {
    semantic: Semantic,
    seen: Vec<Semantic>,
    observation: SourceEventObservation,
    pending: bool,
}
/// Pure adapter; all retained allocations and output count are section/limit bounded.
#[derive(Debug)]
pub struct ScteSourceEventAdapter {
    source_id: String,
    epoch: u64,
    limits: AdapterLimits,
    slots: BTreeMap<(String, u8), Slot>,
    diagnostics: AdapterDiagnostics,
    retained_semantics: usize,
}
impl ScteSourceEventAdapter {
    /// Construct one source/epoch adapter. No internal clock or mapping is retained.
    #[must_use]
    pub fn new(source_id: String, epoch: u64, limits: AdapterLimits) -> Self {
        Self {
            source_id,
            epoch,
            limits,
            slots: BTreeMap::new(),
            diagnostics: AdapterDiagnostics::default(),
            retained_semantics: 0,
        }
    }
    /// Snapshot of bounded diagnostic counters.
    #[must_use]
    pub fn diagnostics(&self) -> AdapterDiagnostics {
        self.diagnostics
    }
    /// Number of retained boundary slots (including unresolved announcements).
    #[must_use]
    pub fn retained_boundaries(&self) -> usize {
        self.slots.len()
    }
    /// Validate a complete section before making any changes, then normalize it.
    /// Only explicit splice insert and segmentation break 0x22/0x23 are supported.
    /// UPIDs are opaque asset correlation, never occurrence identity or vendor proof.
    /// Unknown timestamps remain unknown, including immediate cues.
    ///
    /// # Errors
    /// Returns the SCTE parser error for malformed/CRC-invalid sections, with no effects.
    pub fn ingest_section(
        &mut self,
        bytes: &[u8],
        occurrence: u64,
        mapping: Option<PtsMappingSnapshot>,
    ) -> Result<Vec<SourceEventObservation>, ScteError> {
        let section = SpliceInfoSection::parse(bytes)?;
        let mut out = Vec::new();
        if self.source_id.is_empty() || self.source_id.len() > self.limits.max_source_bytes {
            self.diagnostics.invalid_identity = self.diagnostics.invalid_identity.saturating_add(1);
            return Ok(out);
        }
        if component_only(&section) {
            self.diagnostics.unsupported = self.diagnostics.unsupported.saturating_add(1);
            return Ok(out);
        }
        for event in section.normalized_cue_events() {
            let cue = event.cue;
            let edge = semantic_edge(&event);
            let Some(edge) = edge else {
                self.diagnostics.unsupported = self.diagnostics.unsupported.saturating_add(1);
                continue;
            };
            let namespace = if event.segmentation.is_some() {
                "seg"
            } else {
                "insert"
            };
            let id = format!("{namespace}:{}:{occurrence}", cue.event_id);
            let asset = event
                .segmentation
                .as_ref()
                .and_then(|d| d.upid.as_ref())
                .map(asset_identity);
            let descriptor = marker_descriptor(&event, edge);
            let semantic = Semantic {
                edge,
                pts: if edge == 2 { None } else { cue.pts_time_90k },
                duration: if edge == 0 {
                    cue.break_duration_ns()
                } else {
                    None
                },
                asset,
                descriptor,
            };
            let slot_key = (id.clone(), edge);
            if let Some(old) = self.slots.get(&slot_key) {
                if old.seen.contains(&semantic) {
                    self.diagnostics.duplicates = self.diagnostics.duplicates.saturating_add(1);
                    continue;
                }
            }
            if self.retained_semantics >= self.limits.max_boundaries {
                self.diagnostics.capacity_exceeded =
                    self.diagnostics.capacity_exceeded.saturating_add(1);
                continue;
            }
            let revision = match self.slots.get(&slot_key) {
                Some(s) => s.observation.revision.checked_add(1),
                None => Some(1),
            };
            let Some(revision) = revision else {
                self.diagnostics.revision_exhausted =
                    self.diagnostics.revision_exhausted.saturating_add(1);
                continue;
            };
            let pts = project(semantic.pts, mapping);
            let observation = SourceEventObservation {
                key: SourceEventKey {
                    source_id: self.source_id.clone(),
                    epoch: self.epoch,
                    origin: EventOrigin::Scte35,
                    event_id: id.clone(),
                },
                revision,
                boundary: boundary(&semantic, pts),
                correlation_id: None,
                asset_id: semantic.asset.clone(),
                descriptor: semantic.descriptor.clone(),
            };
            let cancelled = self.slots.contains_key(&(id.clone(), 2)) || edge == 2;
            let pending = semantic.pts.is_some() && pts.is_none() && !cancelled;
            let mut seen = self
                .slots
                .remove(&slot_key)
                .map_or_else(Vec::new, |s| s.seen);
            seen.push(semantic.clone());
            self.retained_semantics += 1;
            self.slots.insert(
                slot_key,
                Slot {
                    seen,
                    semantic,
                    observation: observation.clone(),
                    pending,
                },
            );
            if edge == 2 {
                for ((key, _), s) in &mut self.slots {
                    if key == &id {
                        s.pending = false;
                    }
                }
            }
            out.push(observation);
        }
        Ok(out)
    }
    /// Resolve retained unknown timestamps from an immutable same-epoch anchor.
    /// Already resolved timestamps stay fixed even if later snapshots advance.
    /// Call explicitly when an anchor becomes available (duplicates do not remap).
    pub fn resolve_pending(&mut self, mapping: PtsMappingSnapshot) -> Vec<SourceEventObservation> {
        let mut out = Vec::new();
        for slot in self.slots.values_mut().filter(|s| s.pending) {
            let Some(pts) = project(slot.semantic.pts, Some(mapping)) else {
                continue;
            };
            let Some(revision) = slot.observation.revision.checked_add(1) else {
                self.diagnostics.revision_exhausted =
                    self.diagnostics.revision_exhausted.saturating_add(1);
                continue;
            };
            slot.observation.boundary = boundary(&slot.semantic, Some(pts));
            slot.observation.revision = revision;
            slot.pending = false;
            out.push(slot.observation.clone());
        }
        out
    }
}
/// F4: a component-mode `splice_insert` only splices the listed elementary
/// streams. Component semantics are not represented in source events, so it
/// must never become a whole-source Start/End boundary.
fn component_only(section: &SpliceInfoSection) -> bool {
    matches!(
        section.command,
        crate::scte::splice35::SpliceCommand::Insert(insert) if !insert.program_splice
    )
}

/// Build the bounded marker identity text off-clock, at ingest time, so the
/// bake/overlay path never reparses wire evidence per frame. `edge` is the
/// already-classified semantic edge (0 start, 1 end, 2 cancel); cancellation
/// carries no descriptor (there is nothing new to name).
fn marker_descriptor(
    event: &crate::scte::splice35::NormalizedCueEvent,
    edge: u8,
) -> Option<MarkerDescriptor> {
    if edge == 2 {
        return None;
    }
    if let Some(d) = &event.segmentation {
        let type_id = d.segmentation_type_id?;
        let name = crate::scte::segmentation_type_name(type_id)?.to_owned();
        let mut detail_parts: Vec<String> = Vec::new();
        if let (Some(num), Some(expected)) = (d.segment_num, d.segments_expected) {
            if num > 0 && expected > 0 {
                detail_parts.push(format!("{num}/{expected}"));
            }
        }
        if let Some(upid) = &d.upid {
            if let Some(label) = crate::scte::segmentation_upid_label(upid) {
                detail_parts.push(label);
            }
        }
        let detail = (!detail_parts.is_empty()).then(|| {
            // Cap at 48 chars on a char boundary: UPID text may be non-ASCII.
            detail_parts.join(" ").chars().take(48).collect::<String>()
        });
        Some(MarkerDescriptor {
            name,
            type_id: Some(format!("0x{type_id:02X}")),
            detail,
        })
    } else {
        // A bare splice_insert without segmentation: out_of_network tells us
        // ad-break direction; time_signal with no segmentation carries no
        // naming evidence beyond the generic lifecycle label.
        match event.cue.kind {
            CueKind::SpliceOut => Some(MarkerDescriptor {
                name: "Ad break".to_owned(),
                type_id: Some("splice_insert".to_owned()),
                detail: event
                    .cue
                    .break_duration_ns()
                    .map(|ns| format!("{}s", ns / 1_000_000_000)),
            }),
            CueKind::SpliceIn => Some(MarkerDescriptor {
                name: "Return from break".to_owned(),
                type_id: Some("splice_insert".to_owned()),
                detail: None,
            }),
            CueKind::TimeSignal => None,
        }
    }
}
fn asset_identity(u: &crate::scte::splice35::SegmentationUpid) -> String {
    use std::fmt::Write;
    let mut s = format!("scte-upid:{:02x}:", u.upid_type);
    for b in &u.value {
        let _ = write!(s, "{b:02x}");
    }
    s
}
fn semantic_edge(event: &crate::scte::splice35::NormalizedCueEvent) -> Option<u8> {
    if event.cue.cancel {
        return Some(2);
    }
    if let Some(d) = &event.segmentation {
        match d.segmentation_type_id {
            Some(0x22) => Some(0),
            Some(0x23) => Some(1),
            _ => None,
        }
    } else {
        match event.cue.kind {
            CueKind::SpliceOut => Some(0),
            CueKind::SpliceIn => Some(1),
            _ => None,
        }
    }
}
fn project(raw: Option<u64>, mapping: Option<PtsMappingSnapshot>) -> Option<i64> {
    mapping?
        .project(
            i64::try_from(raw?).ok()?,
            Rational::new(1, 90_000),
            WrapBits::Mpeg33,
        )
        .map(multiview_core::time::MediaTime::as_nanos)
}
fn boundary(s: &Semantic, pts_ns: Option<i64>) -> EventBoundary {
    match s.edge {
        0 => EventBoundary::Start {
            pts_ns,
            duration_ns: s.duration,
        },
        1 => EventBoundary::End { pts_ns },
        _ => EventBoundary::Cancel,
    }
}
