//! Bounded, transport-neutral source evidence tracking, without clocks or effects.
//!
//! Callers supply occurrence identities (not asset identities), normalized signed
//! nanoseconds, and per-boundary revisions. No receipt time implies media time.
//! Retention is explicit: owners advance epoch floors or prune terminal events.
//! Same-epoch pruning requires an owner replay gate; a full tracker rejects new keys.
use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

/// Evidence provenance; origins never implicitly corroborate one another.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventOrigin {
    /// SCTE-35 signalling.
    Scte35,
    /// Timed ID3 metadata.
    TimedId3,
    /// Manifest date-range or equivalent explicit signalling.
    Manifest,
    /// Unverified vendor playlist evidence, never a break boundary.
    VendorMarker {
        /// Vendor namespace.
        vendor: String,
    },
    /// Externally supplied evidence; authentication belongs to the adapter.
    External,
}

/// An occurrence identity scoped to a source timeline and evidence origin.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceEventKey {
    /// Source identity.
    pub source_id: String,
    /// Timeline generation; discontinuities require a new epoch.
    pub epoch: u64,
    /// Evidence origin.
    pub origin: EventOrigin,
    /// Occurrence identity, not a reusable asset/CAID identity.
    pub event_id: String,
}

/// Observed semantic boundary. Missing timestamps remain unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBoundary {
    /// Explicit start announcement.
    Start {
        /// Source presentation nanoseconds.
        pts_ns: Option<i64>,
        /// Expected duration only; never confirms an end.
        duration_ns: Option<i64>,
    },
    /// Explicit end announcement.
    End {
        /// Source presentation nanoseconds.
        pts_ns: Option<i64>,
    },
    /// Explicit cancellation of this occurrence.
    Cancel,
    /// Playlist evidence is present, not proof of an aired break.
    MarkerObserved,
    /// Playlist evidence disappeared, not a break end.
    MarkerAbsent,
}

/// One bounded observation supplied by an ingest/control adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEventObservation {
    /// Occurrence identity.
    pub key: SourceEventKey,
    /// Monotonically increasing revision within each boundary kind.
    pub revision: u64,
    /// Evidence to retain.
    pub boundary: EventBoundary,
    /// Optional cross-origin association; does not merge evidence.
    pub correlation_id: Option<String>,
    /// Optional reusable asset identity, independent of the occurrence key.
    pub asset_id: Option<String>,
}

/// Caller-injected presentation position; freshness is the caller's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresentationClock {
    /// A current, usable source media mapping.
    Fresh {
        /// Source whose position is supplied.
        source_id: String,
        /// Current timeline generation.
        epoch: u64,
        /// Exact presentation nanoseconds.
        position_ns: i64,
    },
    /// A formerly known mapping is no longer fresh.
    Stale,
    /// No presentation mapping exists.
    Unknown,
}

/// Observational lifecycle, never an instruction to perform an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventLifecycle {
    /// An explicit future start exists.
    Announced,
    /// The explicit start has reached presentation.
    Active,
    /// The explicit end has reached presentation.
    Ended,
    /// Explicitly cancelled.
    Cancelled,
    /// This source is now in a different epoch.
    Superseded,
    /// Insufficient or expired timing evidence.
    Uncertain,
    /// Presence-only evidence, without break semantics.
    Observed,
}

/// Strength of the retained end evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndConfidence {
    /// No usable end time.
    Unknown,
    /// Duration predicts an end but does not confirm it.
    Expected,
    /// An explicit, timed end observation exists.
    Confirmed,
}

/// Pure projection at one supplied media position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventProjection {
    /// Projected observational lifecycle.
    pub lifecycle: EventLifecycle,
    /// Expected end derived with checked integer arithmetic.
    pub expected_end_ns: Option<i64>,
    /// Explicit end time if known.
    pub confirmed_end_ns: Option<i64>,
    /// End evidence confidence, not automation eligibility.
    pub end_confidence: EndConfidence,
}

/// Retained evidence for one occurrence, including independently revised edges.
#[derive(Debug, Clone)]
pub struct SourceEvent {
    /// Occurrence identity.
    pub key: SourceEventKey,
    boundaries: BTreeMap<u8, SourceEventObservation>,
}

impl SourceEvent {
    /// Latest evidence per boundary, in start/end/cancel/present/absent order.
    pub fn observations(&self) -> impl Iterator<Item = &SourceEventObservation> {
        self.boundaries.values()
    }

    /// Project without mutating history or sampling any clock.
    #[must_use]
    pub fn project(&self, clock: &PresentationClock) -> EventProjection {
        let (start, duration) = match self.boundaries.get(&0).map(|o| &o.boundary) {
            Some(EventBoundary::Start {
                pts_ns,
                duration_ns,
            }) => (*pts_ns, *duration_ns),
            _ => (None, None),
        };
        let end = match self.boundaries.get(&1).map(|o| &o.boundary) {
            Some(EventBoundary::End { pts_ns }) => *pts_ns,
            _ => None,
        };
        let expected = start.zip(duration).and_then(|(s, d)| s.checked_add(d));
        let mut p = EventProjection {
            lifecycle: EventLifecycle::Uncertain,
            expected_end_ns: expected,
            confirmed_end_ns: end,
            end_confidence: if end.is_some() {
                EndConfidence::Confirmed
            } else if expected.is_some() {
                EndConfidence::Expected
            } else {
                EndConfidence::Unknown
            },
        };
        let invalid_interval = start.zip(end).is_some_and(|(s, e)| e < s);
        if invalid_interval {
            p.end_confidence = EndConfidence::Unknown;
            p.confirmed_end_ns = None;
            p.expected_end_ns = None;
        }
        if self.boundaries.contains_key(&2) {
            p.lifecycle = EventLifecycle::Cancelled;
            return p;
        }
        let position = match clock {
            PresentationClock::Fresh {
                source_id,
                epoch,
                position_ns,
            } if source_id == &self.key.source_id => {
                // Only a STRICTLY newer clock supersedes this occurrence. A
                // delayed older clock says nothing about future-epoch evidence:
                // it projects Uncertain so terminal pruning retains it (F3).
                if *epoch > self.key.epoch {
                    p.lifecycle = EventLifecycle::Superseded;
                    return p;
                }
                if *epoch < self.key.epoch {
                    return p;
                }
                *position_ns
            }
            _ => return p,
        };
        if invalid_interval {
            return p;
        }
        if matches!(self.key.origin, EventOrigin::VendorMarker { .. })
            || (self.boundaries.contains_key(&3) && start.is_none() && end.is_none())
        {
            p.lifecycle = EventLifecycle::Observed;
        } else if end.is_some_and(|e| position >= e) {
            p.lifecycle = EventLifecycle::Ended;
        } else if let Some(start) = start {
            p.lifecycle = if position < start {
                EventLifecycle::Announced
            } else if end.is_none() && expected.is_some_and(|e| position >= e) {
                EventLifecycle::Uncertain
            } else {
                EventLifecycle::Active
            };
        }
        p
    }
}

/// Explicit finite retention and payload limits; zero disables that storage.
#[derive(Debug, Clone, Copy)]
pub struct TrackerLimits {
    /// Maximum retained occurrence keys; full storage rejects new keys.
    pub max_events: usize,
    /// Maximum accepted observations in audit history.
    pub max_history: usize,
    /// Maximum UTF-8 bytes in each supplied string.
    pub max_string_bytes: usize,
}

impl Default for TrackerLimits {
    fn default() -> Self {
        Self {
            max_events: 256,
            max_history: 1024,
            max_string_bytes: 1024,
        }
    }
}

/// Ingestion result, with no side effects beyond bounded tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationOutcome {
    /// New evidence accepted.
    Applied,
    /// Exact latest revision replay; no new history.
    Duplicate,
    /// Older boundary revision ignored.
    Stale,
    /// Same boundary revision has different content.
    Conflict,
    /// New occurrence rejected because capacity is full.
    CapacityExceeded,
    /// Invalid timing, vendor boundary, identity, or oversized payload.
    Invalid,
}

/// Saturating diagnostic counters; no unbounded diagnostic queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerDiagnostics {
    /// Exact repeated observations.
    pub duplicates: u64,
    /// Older revisions rejected.
    pub stale: u64,
    /// Conflicting revisions rejected.
    pub conflicts: u64,
    /// Occurrences rejected at capacity.
    pub capacity_exceeded: u64,
    /// Invalid observations rejected.
    pub invalid: u64,
    /// Audit entries dropped to enforce the history limit.
    pub history_dropped: u64,
}

/// Pure bounded tracker. Owners replace/drop it at their retention boundary.
#[derive(Debug)]
pub struct SourceEventTracker {
    limits: TrackerLimits,
    events: BTreeMap<SourceEventKey, SourceEvent>,
    history: VecDeque<SourceEventObservation>,
    diagnostics: TrackerDiagnostics,
    epoch_floors: BTreeMap<String, u64>,
}

impl SourceEventTracker {
    /// Construct empty storage with explicit bounds.
    #[must_use]
    pub fn new(limits: TrackerLimits) -> Self {
        Self {
            limits,
            events: BTreeMap::new(),
            history: VecDeque::new(),
            diagnostics: TrackerDiagnostics::default(),
            epoch_floors: BTreeMap::new(),
        }
    }

    /// Look up retained evidence.
    #[must_use]
    pub fn get(&self, key: &SourceEventKey) -> Option<&SourceEvent> {
        self.events.get(key)
    }

    /// Iterate retained events in deterministic key order.
    pub fn events(&self) -> impl Iterator<Item = &SourceEvent> {
        self.events.values()
    }

    /// Read bounded accepted-observation history, oldest first.
    #[must_use]
    pub fn history(&self) -> &VecDeque<SourceEventObservation> {
        &self.history
    }

    /// Read aggregate rejection/drop counters.
    #[must_use]
    pub fn diagnostics(&self) -> &TrackerDiagnostics {
        &self.diagnostics
    }

    /// Advance a source's monotonic replay floor and purge older epochs and history.
    ///
    /// Floors use at most `max_events` source identities, each bounded by
    /// `max_string_bytes`. They are never silently evicted. The owner must persist
    /// and restore floors before ingest after restart; epochs must be monotonic.
    /// Returns the number of removed events. No clock is sampled.
    ///
    /// # Errors
    /// Returns `Invalid` for an invalid source, `Stale` for a decreasing floor,
    /// or `CapacityExceeded` when a new source floor cannot be retained.
    pub fn advance_epoch_floor(
        &mut self,
        source_id: &str,
        min_epoch: u64,
    ) -> Result<usize, ObservationOutcome> {
        if source_id.is_empty() || source_id.len() > self.limits.max_string_bytes {
            return Err(ObservationOutcome::Invalid);
        }
        if let Some(old) = self.epoch_floors.get(source_id) {
            if min_epoch < *old {
                return Err(ObservationOutcome::Stale);
            }
        } else if self.epoch_floors.len() >= self.limits.max_events {
            return Err(ObservationOutcome::CapacityExceeded);
        }
        self.epoch_floors.insert(source_id.to_owned(), min_epoch);
        let before = self.events.len();
        self.events
            .retain(|key, _| key.source_id != source_id || key.epoch >= min_epoch);
        self.history
            .retain(|o| o.key.source_id != source_id || o.key.epoch >= min_epoch);
        Ok(before - self.events.len())
    }

    /// Remove terminal events selected by caller policy at an injected position.
    /// Bounded audit history is retained. Active/announced/uncertain/presence-only
    /// events cannot be removed by this method.
    ///
    /// **Replay contract:** same-epoch removed keys have no tombstone here. The
    /// owner must reject replays using its persistent occurrence/idempotency gate
    /// before calling `observe`. Prefer `advance_epoch_floor` when retiring an
    /// entire epoch; it enforces that replay floor inside the tracker. Revision
    /// numbers are per boundary, not a global ingest sequence or replay floor.
    pub fn prune_terminal(
        &mut self,
        clock: &PresentationClock,
        mut select: impl FnMut(&SourceEvent) -> bool,
    ) -> usize {
        let before = self.events.len();
        self.events.retain(|_, event| {
            let terminal = matches!(
                event.project(clock).lifecycle,
                EventLifecycle::Ended | EventLifecycle::Cancelled | EventLifecycle::Superseded
            );
            !terminal || !select(event)
        });
        before - self.events.len()
    }

    /// Accept a normalized observation, deduplicating independently per boundary.
    /// Cancellation is terminal for this occurrence; use a new identity to restart.
    pub fn observe(&mut self, observation: SourceEventObservation) -> ObservationOutcome {
        let slot = boundary_slot(&observation.boundary);
        let outcome = if !valid(&observation, self.limits.max_string_bytes) {
            ObservationOutcome::Invalid
        } else if self
            .epoch_floors
            .get(&observation.key.source_id)
            .is_some_and(|floor| observation.key.epoch < *floor)
        {
            ObservationOutcome::Stale
        } else if let Some(old) = self
            .events
            .get(&observation.key)
            .and_then(|e| e.boundaries.get(&slot))
        {
            match old.revision.cmp(&observation.revision) {
                std::cmp::Ordering::Greater => ObservationOutcome::Stale,
                std::cmp::Ordering::Equal if old == &observation => ObservationOutcome::Duplicate,
                std::cmp::Ordering::Equal => ObservationOutcome::Conflict,
                std::cmp::Ordering::Less => ObservationOutcome::Applied,
            }
        } else if !self.events.contains_key(&observation.key)
            && self.events.len() >= self.limits.max_events
        {
            ObservationOutcome::CapacityExceeded
        } else {
            ObservationOutcome::Applied
        };
        let counter = match outcome {
            ObservationOutcome::Duplicate => Some(&mut self.diagnostics.duplicates),
            ObservationOutcome::Stale => Some(&mut self.diagnostics.stale),
            ObservationOutcome::Conflict => Some(&mut self.diagnostics.conflicts),
            ObservationOutcome::CapacityExceeded => Some(&mut self.diagnostics.capacity_exceeded),
            ObservationOutcome::Invalid => Some(&mut self.diagnostics.invalid),
            ObservationOutcome::Applied => None,
        };
        if let Some(counter) = counter {
            *counter = counter.saturating_add(1);
            return outcome;
        }
        self.events
            .entry(observation.key.clone())
            .or_insert_with(|| SourceEvent {
                key: observation.key.clone(),
                boundaries: BTreeMap::new(),
            })
            .boundaries
            .insert(slot, observation.clone());
        if self.history.len() == self.limits.max_history {
            self.history.pop_front();
            self.diagnostics.history_dropped = self.diagnostics.history_dropped.saturating_add(1);
        }
        if self.limits.max_history != 0 {
            self.history.push_back(observation);
        }
        outcome
    }
}

fn boundary_slot(boundary: &EventBoundary) -> u8 {
    match boundary {
        EventBoundary::Start { .. } => 0,
        EventBoundary::End { .. } => 1,
        EventBoundary::Cancel => 2,
        EventBoundary::MarkerObserved => 3,
        EventBoundary::MarkerAbsent => 4,
    }
}

fn valid(o: &SourceEventObservation, max: usize) -> bool {
    if o.key.source_id.is_empty() || o.key.event_id.is_empty() {
        return false;
    }
    let mut strings = vec![o.key.source_id.as_str(), o.key.event_id.as_str()];
    if let EventOrigin::VendorMarker { vendor } = &o.key.origin {
        if vendor.is_empty()
            || matches!(
                o.boundary,
                EventBoundary::Start { .. } | EventBoundary::End { .. }
            )
        {
            return false;
        }
        strings.push(vendor);
    }
    strings.extend(o.asset_id.as_deref());
    strings.extend(o.correlation_id.as_deref());
    if strings.into_iter().any(|s| s.len() > max) {
        return false;
    }
    if let EventBoundary::Start {
        pts_ns,
        duration_ns: Some(duration),
    } = o.boundary
    {
        if duration < 0 || pts_ns.is_some_and(|s| s.checked_add(duration).is_none()) {
            return false;
        }
    }
    true
}
