//! Integration tests for the resource-scoped log capture layer + bounded ring
//! (ADR-0060 §4.4): a `tracing` event emitted inside a resource span is captured
//! into the bounded drop-oldest ring with its `resource_id` field, the ring drops
//! oldest beyond its capacity, and the filter (`resource_id` / minimum level /
//! since) selects the right records. Logging is best-effort and bounded
//! (invariant #10): the ring never grows past its cap. `resource_id` /
//! `min_level` / `since_seq` are the operator's triage filters.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_telemetry::log_capture::{
    LogCaptureLayer, LogFilter, LogLevel, LogResourceKind, LogRing,
};
use tracing_subscriber::layer::SubscriberExt;

/// Run `body` with a subscriber that has the capture layer installed, returning
/// the shared ring afterwards.
fn with_capture<F: FnOnce()>(ring: &Arc<LogRing>, body: F) {
    let layer = LogCaptureLayer::new(Arc::clone(ring)).with_run_id("run-test");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, body);
}

#[test]
fn event_inside_source_span_captures_resource_id() {
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        let span = tracing::info_span!(
            "source",
            resource_kind = "source",
            resource_id = "cnn",
            label = "CNN"
        );
        let _g = span.enter();
        tracing::error!(target: "libav", component = "hevc", "Error constructing the frame RPS.");
    });

    let records = ring.snapshot();
    assert_eq!(records.len(), 1, "exactly one record captured");
    let rec = &records[0];
    assert_eq!(
        rec.resource_id.as_deref(),
        Some("cnn"),
        "the record inherits the source span's resource_id"
    );
    assert_eq!(rec.resource_kind, Some(LogResourceKind::Source));
    assert_eq!(rec.label.as_deref(), Some("CNN"));
    assert_eq!(rec.run_id.as_deref(), Some("run-test"));
    assert_eq!(rec.level, LogLevel::Error);
    assert_eq!(rec.component.as_deref(), Some("hevc"));
    assert!(
        rec.message.contains("frame RPS"),
        "message body preserved: {:?}",
        rec.message
    );
}

#[test]
fn event_outside_any_resource_span_has_no_resource_id() {
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        tracing::warn!(target: "libav", component = "https", "Opening 'https://host/x' for reading");
    });
    let records = ring.snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].resource_id, None,
        "an unattributed line omits resource_id (honesty over a wrong id)"
    );
    assert_eq!(records[0].resource_kind, None);
}

#[test]
fn event_field_resource_id_is_captured_when_no_span() {
    // A libav line emitted on a libav-owned thread has NO enclosing resource
    // span; the bridge attaches resource_kind/resource_id/label as event fields.
    // The capture layer must read those when the span scope yields nothing.
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        tracing::error!(
            target: "libav",
            component = "hevc",
            resource_kind = "source",
            resource_id = "cnn",
            label = "CNN",
            "Error constructing the frame RPS."
        );
    });
    let records = ring.snapshot();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert_eq!(
        rec.resource_id.as_deref(),
        Some("cnn"),
        "the libav-thread event field is captured as the record's resource_id"
    );
    assert_eq!(rec.resource_kind, Some(LogResourceKind::Source));
    assert_eq!(rec.label.as_deref(), Some("CNN"));
    assert_eq!(rec.component.as_deref(), Some("hevc"));
}

#[test]
fn span_resource_id_wins_over_event_field() {
    // If BOTH a span and an event field carry resource_id, the span (our own
    // attributed code) takes precedence — the event field is the fallback only.
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        let span = tracing::info_span!("source", resource_kind = "source", resource_id = "span-id");
        let _g = span.enter();
        tracing::info!(resource_id = "event-id", "x");
    });
    let records = ring.snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].resource_id.as_deref(), Some("span-id"));
}

#[test]
fn ring_is_bounded_drop_oldest() {
    let ring = Arc::new(LogRing::new(3));
    with_capture(&ring, || {
        for i in 0..10 {
            tracing::info!(seq = i, "line {i}");
        }
    });
    let records = ring.snapshot();
    assert_eq!(records.len(), 3, "ring never exceeds its capacity");
    // The three most-recent lines survive; the oldest seven were dropped.
    assert!(records[0].message.contains("line 7"), "{records:?}");
    assert!(records[2].message.contains("line 9"), "{records:?}");
    assert_eq!(
        ring.dropped(),
        7,
        "the ring counts the records it dropped (inv #10 — lossy, observable)"
    );
}

#[test]
fn filter_by_resource_id_and_minimum_level() {
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        {
            let s = tracing::info_span!("source", resource_kind = "source", resource_id = "cnn");
            let _g = s.enter();
            tracing::info!("cnn info");
            tracing::error!("cnn error");
        }
        {
            let s = tracing::info_span!("source", resource_kind = "source", resource_id = "bbc");
            let _g = s.enter();
            tracing::error!("bbc error");
        }
    });

    // Filter: resource cnn, level >= warn -> only "cnn error".
    let filtered = ring.query(&LogFilter {
        resource_id: Some("cnn".to_owned()),
        min_level: Some(LogLevel::Warn),
        ..LogFilter::default()
    });
    assert_eq!(filtered.len(), 1, "{filtered:?}");
    assert!(filtered[0].message.contains("cnn error"));

    // Filter: resource bbc, no level floor -> the single bbc record.
    let bbc = ring.query(&LogFilter {
        resource_id: Some("bbc".to_owned()),
        ..LogFilter::default()
    });
    assert_eq!(bbc.len(), 1);
    assert!(bbc[0].message.contains("bbc error"));
}

#[test]
fn filter_since_sequence_returns_only_newer_records() {
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        tracing::info!("first");
        tracing::info!("second");
        tracing::info!("third");
    });
    let all = ring.snapshot();
    assert_eq!(all.len(), 3);
    let cutoff = all[0].seq;
    let newer = ring.query(&LogFilter {
        since_seq: Some(cutoff),
        ..LogFilter::default()
    });
    assert_eq!(newer.len(), 2, "records strictly after the cutoff seq");
    assert!(newer[0].message.contains("second"));
    assert!(newer[1].message.contains("third"));
}

#[test]
fn limit_returns_most_recent_n() {
    let ring = Arc::new(LogRing::new(64));
    with_capture(&ring, || {
        for i in 0..10 {
            tracing::info!("line {i}");
        }
    });
    let last3 = ring.query(&LogFilter {
        limit: Some(3),
        ..LogFilter::default()
    });
    assert_eq!(last3.len(), 3);
    assert!(last3[0].message.contains("line 7"));
    assert!(last3[2].message.contains("line 9"));
}

#[test]
fn on_record_hook_observes_every_captured_record() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ring = Arc::new(LogRing::new(64));
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = Arc::clone(&count);
    let layer = LogCaptureLayer::new(Arc::clone(&ring)).with_on_record(move |_rec| {
        count2.fetch_add(1, Ordering::Relaxed);
    });
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("a");
        tracing::info!("b");
    });
    assert_eq!(
        count.load(Ordering::Relaxed),
        2,
        "the live-tail hook fires once per captured record"
    );
}
