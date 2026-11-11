//! Read-only source-event bridge. The binary supplies the runtime adapter.
use serde::Serialize;
use std::sync::Arc;

/// Maximum event rows in one response.
pub const MAX_SOURCE_EVENTS: usize = 256;
/// Maximum bytes in any public identity token. Oversized identities are omitted,
/// never truncated into a different identity.
pub const MAX_SOURCE_EVENT_ID_BYTES: usize = 256;

/// A bounded snapshot provider, independent of CLI implementation details.
///
/// Implementations must project events against the exact latched source-frame
/// clock and its epoch, never the latest ingest anchor. Missing/stale frame timing
/// must report uncertainty, not a fabricated countdown. Reads must be bounded and
/// must never block the engine. Return `None` only for an unknown source; known
/// sources without events return an empty snapshot. Publish only public identity
/// tokens and semantic fields, never raw cue bytes, URLs, credentials or secrets.
pub trait SourceEventProvider: Send + Sync {
    /// Read the observational snapshot for this exact source identity.
    fn source_events(&self, id: &str) -> Option<SourceEventsDto>;
}
/// Shared optional runtime adapter handle.
pub type SharedSourceEvents = Arc<dyn SourceEventProvider>;

/// Operator test-injection kind (ADR-0183 item 5): a caller-selected synthetic
/// lifecycle boundary, independent of the CLI's internal representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestEventBoundary {
    /// Announce/confirm a synthetic break start.
    Start {
        /// Declared duration in nanoseconds, if supplied.
        duration_ns: Option<i64>,
    },
    /// Confirm a synthetic break end for the most recent injected start.
    End,
}

/// Outcome of a test-injection submission, independent of CLI internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestEventOutcome {
    /// Accepted into the bounded worker queue.
    Queued,
    /// Queue full; this source's evidence is now permanently degraded.
    Overflow,
    /// Source has no active generation (never ingested, or retired).
    Unknown,
}

/// Optional adapter for the operator test-injection route. Implemented ONLY by
/// the runtime binary over its real `SourceEventRegistry`; never exposed to an
/// unauthenticated request body and never a general evidence-submission API —
/// this can only affect a source that already has an active generation.
pub trait SourceEventTestInjector: Send + Sync {
    /// Submit a synthetic observation against the source's current generation.
    fn inject_test_event(&self, id: &str, boundary: TestEventBoundary) -> TestEventOutcome;
}
/// Shared optional runtime test-injection handle.
pub type SharedSourceEventTestInjector = Arc<dyn SourceEventTestInjector>;

/// Availability of the latched presentation clock, not ingest health.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SourceEventAvailability {
    /// Exact latched source-frame clock is usable.
    Available,
    /// Previously usable presentation clock is stale.
    Stale,
    /// No presentation clock exists yet.
    Unavailable,
}

/// Structured observational row. No raw transport payload is exposed.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceEventDto {
    /// Public occurrence identity, not an asset identity.
    pub event_id: String,
    /// Exact unsigned decimal timeline epoch.
    pub epoch: String,
    /// Evidence origin; vendor markers never imply a splice boundary.
    pub origin: SourceEventOrigin,
    /// Public vendor namespace when applicable.
    pub vendor: Option<String>,
    /// Observational lifecycle, not an automation command.
    pub lifecycle: SourceEventLifecycle,
    /// Server-projected source-media timer; clients must not tick it from receipt time.
    pub timer: Option<SourceEventTimer>,
    /// Bounded public asset identity, independent of this occurrence.
    #[cfg_attr(feature = "openapi", schema(max_length = 256))]
    pub asset_id: Option<String>,
    /// Bounded public association token; never proof that origins corroborate.
    #[cfg_attr(feature = "openapi", schema(max_length = 256))]
    pub correlation_id: Option<String>,
    /// Optional semantic display label consistent with lifecycle and timer.
    pub label: Option<SourceEventLabel>,
    /// Short human-readable marker name (e.g. "Program Start", "Ad break"),
    /// when the evidence carried enough identity to name it. Absent means
    /// this marker is honestly unidentified, not that it was omitted.
    #[cfg_attr(feature = "openapi", schema(max_length = 48))]
    pub marker_name: Option<String>,
    /// Raw wire type token the name was derived from (e.g. `"0x22"`,
    /// `"splice_insert"`, `"PRIV"`, `"asset"`).
    #[cfg_attr(feature = "openapi", schema(max_length = 32))]
    pub marker_type_id: Option<String>,
    /// Short additional detail (segment ordinal, break duration, UPID
    /// suffix), consistent with `marker_name`.
    #[cfg_attr(feature = "openapi", schema(max_length = 48))]
    pub marker_detail: Option<String>,
}

/// Exact timer value projected by the server against the selected source frame.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceEventTimer {
    /// Interpretation of the nonnegative duration.
    pub kind: SourceEventTimerKind,
    /// Canonical unsigned decimal nanoseconds, never a floating-point JSON number.
    #[cfg_attr(
        feature = "openapi",
        schema(max_length = 20, pattern = "^(0|[1-9][0-9]{0,19})$")
    )]
    pub nanoseconds: String,
}

/// Meaning of a server-projected timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SourceEventTimerKind {
    /// Time until the announced start.
    UntilStart,
    /// Time until an expected or confirmed end.
    Remaining,
    /// Time since an explicit start when no end is known.
    Elapsed,
    /// Time past the expected end; the end remains unconfirmed.
    Overrun,
}

/// Semantic badge label, independent of evidence origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SourceEventLabel {
    /// Announced start is approaching.
    Upcoming,
    /// Explicit start has been presented.
    Active,
    /// Explicit end has been presented.
    Ended,
    /// Timing is uncertain.
    Uncertain,
    /// Expected end has passed without confirmation.
    Overrun,
    /// Presence-only metadata, not necessarily vendor evidence.
    Observed,
}

/// Independent evidence origin.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SourceEventOrigin {
    /// SCTE-35 signalling.
    Scte35,
    /// Timed ID3.
    TimedId3,
    /// Manifest signalling.
    Manifest,
    /// Unverified vendor presence evidence.
    VendorMarker,
    /// Authenticated external evidence.
    External,
}
/// Source-relative observational lifecycle.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum SourceEventLifecycle {
    /// Future start.
    Announced,
    /// Presented start.
    Active,
    /// Presented explicit end.
    Ended,
    /// Cancelled occurrence.
    Cancelled,
    /// Invalidated epoch.
    Superseded,
    /// Insufficient timing.
    Uncertain,
    /// Presence only.
    Observed,
}

fn canonical_decimal(value: &str) -> bool {
    value.len() <= 20 && value.parse::<u64>().is_ok_and(|n| n.to_string() == value)
}

fn valid_event(event: &SourceEventDto, availability: SourceEventAvailability) -> bool {
    if event.event_id.is_empty()
        || event.event_id.len() > MAX_SOURCE_EVENT_ID_BYTES
        || !canonical_decimal(&event.epoch)
        || [&event.vendor, &event.asset_id, &event.correlation_id]
            .into_iter()
            .flatten()
            .any(|value| value.len() > MAX_SOURCE_EVENT_ID_BYTES)
        || event.marker_name.as_ref().is_some_and(|v| v.len() > 48)
        || event.marker_type_id.as_ref().is_some_and(|v| v.len() > 32)
        || event.marker_detail.as_ref().is_some_and(|v| v.len() > 48)
    {
        return false;
    }
    if matches!(event.origin, SourceEventOrigin::VendorMarker)
        && (event.vendor.as_ref().is_none_or(String::is_empty)
            || !matches!(
                event.lifecycle,
                SourceEventLifecycle::Observed
                    | SourceEventLifecycle::Uncertain
                    | SourceEventLifecycle::Cancelled
                    | SourceEventLifecycle::Superseded
            ))
    {
        return false;
    }
    let timer_kind = event.timer.as_ref().map(|timer| timer.kind);
    if let Some(timer) = &event.timer {
        if !matches!(availability, SourceEventAvailability::Available)
            || matches!(event.origin, SourceEventOrigin::VendorMarker)
            || !canonical_decimal(&timer.nanoseconds)
            || !matches!(
                (event.lifecycle, timer.kind),
                (
                    SourceEventLifecycle::Announced,
                    SourceEventTimerKind::UntilStart
                ) | (
                    SourceEventLifecycle::Active,
                    SourceEventTimerKind::Remaining | SourceEventTimerKind::Elapsed
                ) | (
                    SourceEventLifecycle::Uncertain,
                    SourceEventTimerKind::Overrun
                )
            )
        {
            return false;
        }
    }
    event.label.is_none_or(|label| {
        matches!(
            (event.lifecycle, label),
            (SourceEventLifecycle::Announced, SourceEventLabel::Upcoming)
                | (SourceEventLifecycle::Active, SourceEventLabel::Active)
                | (SourceEventLifecycle::Ended, SourceEventLabel::Ended)
                | (SourceEventLifecycle::Uncertain, SourceEventLabel::Uncertain)
                | (SourceEventLifecycle::Observed, SourceEventLabel::Observed)
        ) || (matches!(event.lifecycle, SourceEventLifecycle::Uncertain)
            && label == SourceEventLabel::Overrun
            && timer_kind == Some(SourceEventTimerKind::Overrun))
    })
}

/// Bounded source snapshot with exact browser-safe epoch identities.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceEventsDto {
    /// Exact source identity requested by the caller.
    source_id: String,
    /// Exact unsigned decimal epoch, including values above 2^53.
    epoch: String,
    /// Presentation clock availability.
    availability: SourceEventAvailability,
    /// True if upstream evidence or this response lost information.
    degraded: bool,
    /// Bounded structured event history; an empty list is valid.
    #[cfg_attr(feature = "openapi", schema(max_items = 256))]
    events: Vec<SourceEventDto>,
    /// The source's latest embedded wall clock, when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_clock: Option<SourceClockDto>,
    /// Newest decoded markers first, with predicted times.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(max_items = 32))]
    markers: Option<Vec<SourceMarkerDto>>,
}

/// The source's embedded wall clock. A clock reference, never an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceClockDto {
    /// Latest decoded source UTC time.
    pub utc: String,
    /// Source clock minus host receive time, in milliseconds. Includes the
    /// stream's delivery delay, so a live source reads negative.
    pub offset_ms: i64,
    /// Which embedded clock this is.
    pub clock_source: crate::event_journal::JournalClockSource,
    /// Host time of that reading.
    pub observed_at: String,
}

/// One decoded marker with predicted times. Display evidence only: lifecycle
/// and automation stay on [`SourceEventDto`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SourceMarkerDto {
    /// Journal record holding the full decode.
    pub record_id: String,
    /// Short name.
    pub name: String,
    /// Host receive time.
    pub received_at: String,
    /// Predicted splice wall-clock on the source, UTC.
    pub start_at: Option<String>,
    /// Predicted break end on the source, UTC.
    pub end_at: Option<String>,
    /// Predicted instant the splice reaches this multiview's output, UTC.
    pub airs_at: Option<String>,
    /// Predicted instant the break end reaches this multiview's output, UTC.
    pub airs_end_at: Option<String>,
    /// Declared duration.
    pub duration_ms: Option<u64>,
    /// Clock used for the wall-clock fields.
    pub clock_source: Option<crate::event_journal::JournalClockSource>,
    /// Estimated uncertainty of that clock.
    pub uncertainty_ms: Option<u64>,
    /// `splice_event_id` or `segmentation_event_id`.
    pub event_id: Option<String>,
    /// Decoded UPID with its type name.
    pub upid: Option<String>,
}
impl SourceEventsDto {
    /// Construct a snapshot, dropping oversized/invalid rows and excess history.
    /// The route verifies the exact source identity before exposing the result.
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        epoch: u64,
        availability: SourceEventAvailability,
        degraded: bool,
        mut events: Vec<SourceEventDto>,
    ) -> Self {
        let original_len = events.len();
        events.retain(|event| valid_event(event, availability));
        events.truncate(MAX_SOURCE_EVENTS);
        Self {
            source_id: source_id.into(),
            epoch: epoch.to_string(),
            availability,
            degraded: degraded || events.len() != original_len,
            events,
            source_clock: None,
            markers: None,
        }
    }
    /// Attach the decoded clock and marker summary.
    #[must_use]
    pub fn with_decoded(
        mut self,
        source_clock: Option<SourceClockDto>,
        markers: Vec<SourceMarkerDto>,
    ) -> Self {
        self.source_clock = source_clock;
        self.markers = Some(markers);
        self
    }
    pub(crate) fn matches_source(&self, id: &str) -> bool {
        self.source_id == id && self.source_id.len() <= MAX_SOURCE_EVENT_ID_BYTES
    }
}

/// Break state of one source for the automation executor (ADR-0183), derived
/// from reliable explicit evidence only (never vendor presence markers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakStatus {
    /// Exact service epoch the status was derived from.
    pub epoch: u64,
    /// The occurrence currently in break, if any (presented start, no end).
    pub active: Option<ActiveBreak>,
    /// Evidence loss is latched: never start a new automated action.
    pub degraded: bool,
}
/// One break occurrence currently presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBreak {
    /// Occurrence identity (origin-qualified, never an asset id).
    pub event_id: String,
    /// Whether this is operator-injected test evidence.
    pub test: bool,
}
/// Management-plane break-status adapter, implemented by the runtime binary.
/// Reads are bounded and lock-free on the engine side; never called on the
/// output clock.
pub trait SourceBreakProvider: Send + Sync {
    /// Reacquire and evaluate the current break status of `id`. Every call is
    /// a FRESH acquisition, so calling it immediately before an effect is the
    /// S1 revalidation (a cancellation/revision published since an earlier
    /// call changes the answer).
    fn break_status(&self, id: &str) -> Option<BreakStatus>;
}
/// Shared optional break-status adapter.
pub type SharedSourceBreakProvider = Arc<dyn SourceBreakProvider>;
