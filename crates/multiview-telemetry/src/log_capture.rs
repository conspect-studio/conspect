//! Resource-scoped structured log capture: the bounded, drop-oldest log ring and
//! the `tracing` [`LogCaptureLayer`] that feeds it (ADR-0060 §4.4).
//!
//! Every `tracing` event the process emits — ours and the libav bridge's
//! (`multiview-ffmpeg`'s `log_bridge`) — is mirrored into a fixed-capacity ring of
//! structured [`LogRecord`]s. Each record inherits the `resource_kind` /
//! `resource_id` / `label` carried by the **resource span** it was emitted inside
//! (`source{…}` / `output{…}` / `layout{…}` / `program{…}`, ADR-0060 §2.1), so a
//! decode error names *which* source even though libav has no idea about our
//! config ids. The ring is the producer behind the control-plane `GET /api/v1/logs`
//! tail and the `Topic::Logs` live stream.
//!
//! ## Isolation (invariant #10)
//!
//! The ring is **bounded drop-oldest**: a full ring evicts its oldest record and
//! counts the drop; a slow `GET /logs` reader or a wedged WebSocket subscriber
//! loses old lines, never stalls an emitter. The ring's lock is a `std::sync::Mutex`
//! held only for the O(1) push / O(len) snapshot scan — it is **never** shared with
//! the engine, and the engine does not log per tick (ADR-0060 §4.1), so this path
//! is off the data plane by construction. The optional [`on_record`](LogCaptureLayer::with_on_record)
//! hook (used by the control plane to publish onto its `tokio::broadcast`) runs
//! inline on the emitting thread but must itself be non-blocking and lossy — the
//! control plane wires a `broadcast::Sender` whose send is drop-on-lag.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// The kind of resource a log record is attributed to (ADR-0060 §2.2).
///
/// A stable, closed vocabulary shared with alarms (MV001) and per-resource
/// stats — the discriminator for the `resource_id`. `#[non_exhaustive]` so a
/// finer kind (e.g. a sync-group) can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LogResourceKind {
    /// An ingest source (`Source.id`).
    Source,
    /// An output sink (`Output.id`).
    Output,
    /// A layout / hot-reconfig operation (`Layout.id`).
    Layout,
    /// The protected output core's own rare program-level events.
    Program,
    /// A managed device (`Device.id`).
    Device,
}

impl LogResourceKind {
    /// Parse the span-field / wire string form (`"source"`, `"output"`, …).
    ///
    /// Returns [`None`] for an unrecognised value so an unknown span field never
    /// silently masquerades as a real kind.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "source" => Some(Self::Source),
            "output" => Some(Self::Output),
            "layout" => Some(Self::Layout),
            "program" => Some(Self::Program),
            "device" => Some(Self::Device),
            _ => None,
        }
    }

    /// The lowercase wire string for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Output => "output",
            Self::Layout => "layout",
            Self::Program => "program",
            Self::Device => "device",
        }
    }
}

/// The severity band of a captured record — the five `tracing` levels collapsed
/// into a serializable, totally-ordered enum (ERROR most severe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// `tracing::Level::TRACE`.
    Trace,
    /// `tracing::Level::DEBUG`.
    Debug,
    /// `tracing::Level::INFO`.
    Info,
    /// `tracing::Level::WARN`.
    Warn,
    /// `tracing::Level::ERROR`.
    Error,
}

impl LogLevel {
    /// Map a `tracing::Level` to a [`LogLevel`].
    #[must_use]
    pub fn from_tracing(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }

    /// Parse a case-insensitive level name (`"warn"`, `"error"`, …).
    ///
    /// Returns [`None`] for an unrecognised name so an operator's typo'd filter
    /// is rejected (a `422`) rather than silently ignored.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// The lowercase wire string for this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One structured log record in the bounded ring (ADR-0060 §2.3).
///
/// Carries the span-inherited resource attribution (`run_id` / `resource_kind` /
/// `resource_id` / `label`), the event's own `level` / `target` / `message`, the
/// libav `component` and coalesced `repeated` fields where present, a monotonic
/// `seq` for cursor paging, and a millisecond wall-clock `timestamp_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Monotonic capture sequence number (the cursor for `since`).
    pub seq: u64,
    /// Wall-clock capture time, milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The record's severity.
    pub level: LogLevel,
    /// The `tracing` target (e.g. `"libav"`, `"multiview_engine"`).
    pub target: String,
    /// The rendered event message.
    pub message: String,
    /// The process run id (`run{run_id}` span), if a run id was configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The resource kind this record is attributed to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_kind: Option<LogResourceKind>,
    /// The stable config resource id this record is attributed to, if any.
    ///
    /// Omitted (not guessed) when attribution could not be resolved — honesty
    /// over a wrong id (ADR-0060 §3.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// The resource's human label, if the span carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The libav component class (`"hevc"`, `"hls"`), for bridge records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// The coalesced suppressed-repeat count, when a bridge record flushes a
    /// summary (`repeated = N`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeated: Option<u64>,
}

/// A filter for [`LogRing::query`] — the `GET /api/v1/logs` query translated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Keep only records attributed to this resource id.
    pub resource_id: Option<String>,
    /// Keep only records of this resource kind.
    pub resource_kind: Option<LogResourceKind>,
    /// Keep only records at or above this severity.
    pub min_level: Option<LogLevel>,
    /// Keep only records with `seq` strictly greater than this cursor.
    pub since_seq: Option<u64>,
    /// Return at most this many of the most-recent matching records.
    pub limit: Option<usize>,
}

impl LogFilter {
    fn matches(&self, rec: &LogRecord) -> bool {
        if let Some(id) = &self.resource_id {
            if rec.resource_id.as_deref() != Some(id.as_str()) {
                return false;
            }
        }
        if let Some(kind) = self.resource_kind {
            if rec.resource_kind != Some(kind) {
                return false;
            }
        }
        if let Some(min) = self.min_level {
            if rec.level < min {
                return false;
            }
        }
        if let Some(cursor) = self.since_seq {
            if rec.seq <= cursor {
                return false;
            }
        }
        true
    }
}

/// A fixed-capacity, drop-oldest ring of [`LogRecord`]s (ADR-0060 §4.4).
///
/// Bounded (invariant #10): pushing into a full ring evicts the oldest record and
/// increments [`dropped`](LogRing::dropped). The internal lock is held only for the
/// O(1) push / O(len) read; it is never shared with the engine.
#[derive(Debug)]
pub struct LogRing {
    inner: Mutex<VecDeque<LogRecord>>,
    capacity: usize,
    seq: AtomicU64,
    dropped: AtomicU64,
}

impl LogRing {
    /// Create a ring retaining at most `capacity` records (clamped to ≥1 so a
    /// degenerate `0` never produces a ring that drops everything it captures).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            seq: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Allocate the next monotonic capture sequence number.
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Push a record, dropping the oldest if at capacity (never blocks, never
    /// grows). A poisoned lock is recovered rather than propagated — a logging
    /// store must never crash a writer.
    pub fn push(&self, record: LogRecord) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() >= self.capacity {
            let _evicted = guard.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        guard.push_back(record);
    }

    /// How many records have been dropped over the ring's lifetime (inv #10 —
    /// loss is observable, not silent).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// A snapshot of every retained record, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<LogRecord> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.iter().cloned().collect()
    }

    /// The matching records (oldest first), applying `filter`'s predicate then
    /// its `limit` (keeping the most-recent `limit` matches).
    #[must_use]
    pub fn query(&self, filter: &LogFilter) -> Vec<LogRecord> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut matched: Vec<LogRecord> = guard
            .iter()
            .filter(|r| filter.matches(r))
            .cloned()
            .collect();
        if let Some(limit) = filter.limit {
            if matched.len() > limit {
                let drop = matched.len() - limit;
                matched.drain(0..drop);
            }
        }
        matched
    }
}

/// The resource attribution gleaned from a span's fields, stored as span
/// extension data so child events inherit it without re-parsing.
#[derive(Debug, Clone, Default)]
struct SpanResource {
    run_id: Option<String>,
    resource_kind: Option<LogResourceKind>,
    resource_id: Option<String>,
    label: Option<String>,
}

/// A `tracing` field visitor that extracts the resource-attribution fields from
/// a span's `Attributes` (`run_id` / `resource_kind` / `resource_id` / `label`).
struct SpanFieldVisitor {
    res: SpanResource,
}

impl SpanFieldVisitor {
    fn new() -> Self {
        Self {
            res: SpanResource::default(),
        }
    }
}

impl Visit for SpanFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "run_id" => self.res.run_id = Some(value.to_owned()),
            "resource_kind" => self.res.resource_kind = LogResourceKind::parse(value),
            "resource_id" => self.res.resource_id = Some(value.to_owned()),
            "label" => self.res.label = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // String span fields surface through `record_str`; this catches values
        // recorded via the `Debug` path (e.g. `%expr`/`?expr` formatting) and
        // strips a single layer of debug quoting so `resource_id = %id` still
        // yields a clean id.
        let rendered = format!("{value:?}");
        let cleaned = rendered.trim_matches('"');
        match field.name() {
            "run_id" => self.res.run_id = Some(cleaned.to_owned()),
            "resource_kind" => self.res.resource_kind = LogResourceKind::parse(cleaned),
            "resource_id" => self.res.resource_id = Some(cleaned.to_owned()),
            "label" => self.res.label = Some(cleaned.to_owned()),
            _ => {}
        }
    }
}

/// A `tracing` field visitor that extracts an event's own fields: the `message`,
/// the libav `component`, the coalesced `repeated` count, and — for libav lines
/// emitted on a libav-owned thread (no enclosing resource span) — the resource
/// attribution carried directly as event fields by the bridge (ADR-0060 §3).
struct EventFieldVisitor {
    message: String,
    component: Option<String>,
    repeated: Option<u64>,
    resource: SpanResource,
}

impl EventFieldVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            component: None,
            repeated: None,
            resource: SpanResource::default(),
        }
    }
}

impl Visit for EventFieldVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "repeated" {
            self.repeated = Some(value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "repeated" && value >= 0 {
            self.repeated = Some(value.unsigned_abs());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => value.clone_into(&mut self.message),
            "component" => self.component = Some(value.to_owned()),
            "resource_kind" => self.resource.resource_kind = LogResourceKind::parse(value),
            "resource_id" => self.resource.resource_id = Some(value.to_owned()),
            "label" => self.resource.label = Some(value.to_owned()),
            "run_id" => self.resource.run_id = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        match name {
            "message" => self.message = format!("{value:?}"),
            "component" => self.component = Some(format!("{value:?}").trim_matches('"').to_owned()),
            "resource_kind" | "resource_id" | "label" | "run_id" => {
                let cleaned = format!("{value:?}");
                let cleaned = cleaned.trim_matches('"');
                match name {
                    "resource_kind" => {
                        self.resource.resource_kind = LogResourceKind::parse(cleaned);
                    }
                    "resource_id" => self.resource.resource_id = Some(cleaned.to_owned()),
                    "label" => self.resource.label = Some(cleaned.to_owned()),
                    "run_id" => self.resource.run_id = Some(cleaned.to_owned()),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// A boxed, thread-safe per-record hook (used by the control plane to publish
/// each captured record onto its `tokio::broadcast` for the live tail).
type OnRecord = Arc<dyn Fn(&LogRecord) + Send + Sync>;

/// The `tracing` [`Layer`] that captures every event into a [`LogRing`] with its
/// resource attribution resolved from the enclosing span scope.
///
/// On span creation it parses the span's resource fields once and stores them as
/// a span extension; on each event it walks the scope (nearest span first) for
/// the first span carrying attribution, builds a [`LogRecord`], pushes it to the
/// ring, and fires the optional [`on_record`](Self::with_on_record) hook.
pub struct LogCaptureLayer {
    ring: Arc<LogRing>,
    run_id: Option<String>,
    on_record: Option<OnRecord>,
}

impl std::fmt::Debug for LogCaptureLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogCaptureLayer")
            .field("run_id", &self.run_id)
            .field("has_on_record", &self.on_record.is_some())
            .finish_non_exhaustive()
    }
}

impl LogCaptureLayer {
    /// Create a capture layer feeding `ring`.
    #[must_use]
    pub fn new(ring: Arc<LogRing>) -> Self {
        Self {
            ring,
            run_id: None,
            on_record: None,
        }
    }

    /// Set the process `run_id` stamped on every record (ADR-0060 §2.2). A span
    /// that carries its own `run_id` field overrides this fallback.
    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    /// Set a per-record hook invoked inline for every captured record (the
    /// control plane uses this to publish onto its drop-on-lag broadcast for the
    /// live tail). The hook **must** be non-blocking and lossy — it runs on the
    /// emitting thread and must not back-pressure it (invariant #10).
    #[must_use]
    pub fn with_on_record<F>(mut self, hook: F) -> Self
    where
        F: Fn(&LogRecord) + Send + Sync + 'static,
    {
        self.on_record = Some(Arc::new(hook));
        self
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

impl<S> Layer<S> for LogCaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = SpanFieldVisitor::new();
        attrs.record(&mut visitor);
        // Only store attribution when the span actually carries any — keeps the
        // scope walk cheap and unambiguous.
        if visitor.res.resource_id.is_some()
            || visitor.res.resource_kind.is_some()
            || visitor.res.run_id.is_some()
            || visitor.res.label.is_some()
        {
            span.extensions_mut().insert(visitor.res);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Resolve attribution from the nearest enclosing span carrying it.
        let mut resolved = SpanResource::default();
        if let Some(scope) = ctx.event_scope(event) {
            // `scope` yields nearest-first; the first span with a field wins for
            // that field, so iterate nearest→root and only fill empty slots.
            for span in scope {
                let ext = span.extensions();
                if let Some(res) = ext.get::<SpanResource>() {
                    if resolved.resource_kind.is_none() {
                        resolved.resource_kind = res.resource_kind;
                    }
                    if resolved.resource_id.is_none() {
                        resolved.resource_id.clone_from(&res.resource_id);
                    }
                    if resolved.label.is_none() {
                        resolved.label.clone_from(&res.label);
                    }
                    if resolved.run_id.is_none() {
                        resolved.run_id.clone_from(&res.run_id);
                    }
                }
            }
        }

        let mut ev = EventFieldVisitor::new();
        event.record(&mut ev);

        // A libav line emitted on a libav-owned thread has no enclosing resource
        // span, so the bridge attaches attribution as event fields. Prefer the
        // span scope (our own code) and fall back to the event fields (the
        // bridge) — never overwrite a span-resolved id with an empty event field.
        if resolved.resource_kind.is_none() {
            resolved.resource_kind = ev.resource.resource_kind;
        }
        if resolved.resource_id.is_none() {
            resolved.resource_id = ev.resource.resource_id;
        }
        if resolved.label.is_none() {
            resolved.label = ev.resource.label;
        }
        if resolved.run_id.is_none() {
            resolved.run_id = ev.resource.run_id;
        }

        let meta = event.metadata();
        let record = LogRecord {
            seq: self.ring.next_seq(),
            timestamp_ms: Self::now_ms(),
            level: LogLevel::from_tracing(meta.level()),
            target: meta.target().to_owned(),
            message: ev.message,
            run_id: resolved.run_id.or_else(|| self.run_id.clone()),
            resource_kind: resolved.resource_kind,
            resource_id: resolved.resource_id,
            label: resolved.label,
            component: ev.component,
            repeated: ev.repeated,
        };

        if let Some(hook) = &self.on_record {
            hook(&record);
        }
        self.ring.push(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_parse_round_trips() {
        for k in [
            LogResourceKind::Source,
            LogResourceKind::Output,
            LogResourceKind::Layout,
            LogResourceKind::Program,
            LogResourceKind::Device,
        ] {
            assert_eq!(LogResourceKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LogResourceKind::parse("bogus"), None);
    }

    #[test]
    fn level_ordering_is_severity() {
        assert!(LogLevel::Error > LogLevel::Warn);
        assert!(LogLevel::Warn > LogLevel::Info);
        assert!(LogLevel::Info > LogLevel::Debug);
        assert!(LogLevel::Debug > LogLevel::Trace);
    }

    #[test]
    fn level_parse_accepts_warning_alias() {
        assert_eq!(LogLevel::parse("WARNING"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("nope"), None);
    }

    #[test]
    fn ring_clamps_zero_capacity_to_one() {
        let ring = LogRing::new(0);
        ring.push(LogRecord {
            seq: 0,
            timestamp_ms: 0,
            level: LogLevel::Info,
            target: "t".to_owned(),
            message: "m".to_owned(),
            run_id: None,
            resource_kind: None,
            resource_id: None,
            label: None,
            component: None,
            repeated: None,
        });
        assert_eq!(ring.snapshot().len(), 1);
    }
}
