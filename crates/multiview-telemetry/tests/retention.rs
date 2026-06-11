//! Tests for the consent-independent local metrics **retention store** (CONSPECT
//! engine-seam S5; ADR-0052 §3, conspect-account-architecture §7.2).
//!
//! The store keeps a rolling, bounded, minute-bucketed window of the §7.2 support
//! categories — utilisation samples, shed-load events, per-input reconnect
//! history, and incident markers — for at least seven days, with drop-oldest
//! pruning so memory is bounded forever (data-plane rule 5). Writes are non-
//! blocking so a slow/contended reader can never back-pressure a writer
//! (invariant #10). These tests pin: bucket rollover + drop-oldest at capacity,
//! windowed (1h/24h/7d) queries returning only in-window data, the writer-never-
//! blocks property under contention, and that the store is genuinely independent
//! of any telemetry-consent flag (there is no consent input here at all).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use multiview_telemetry::retention::{
    IncidentKind, RetentionStore, RetentionWindow, ShedReason, BUCKET_SECONDS, RETENTION_BUCKETS,
};

/// [`RETENTION_BUCKETS`] as a `u64` (no `as` cast — the workspace bans them).
fn buckets_u64() -> u64 {
    u64::try_from(RETENTION_BUCKETS).expect("RETENTION_BUCKETS fits u64")
}

/// One minute in seconds, the bucket resolution.
const MINUTE: u64 = 60;
/// One hour in seconds.
const HOUR: u64 = 60 * MINUTE;
/// One day in seconds.
const DAY: u64 = 24 * HOUR;

#[test]
fn capacity_is_at_least_seven_days_of_minute_buckets() {
    // The store must hold >= 7 days at minute resolution: 7 * 24 * 60 = 10_080.
    assert_eq!(BUCKET_SECONDS, MINUTE, "minute-resolution buckets");
    // A compile-time capacity contract: the store must retain at least 7 days of
    // minute buckets (7 * 24 * 60 = 10_080). A `const` block makes the floor a
    // build-time guarantee, not just a run-time check.
    const {
        assert!(
            RETENTION_BUCKETS >= 7 * 24 * 60,
            "must retain at least 7 days of minute buckets"
        );
    }
}

#[test]
fn utilisation_query_returns_only_in_window_samples() {
    let store = RetentionStore::new();
    // "now" anchor for the test, in unix seconds.
    let now = 1_000 * DAY;

    // A sample 8 days ago (outside even the 7d window), one 2 days ago (inside 7d,
    // outside 24h), one 2 hours ago (inside 24h, outside 1h), and one 1 minute ago.
    store.record_utilisation_at(now - 8 * DAY, util(0.10, 0.10));
    store.record_utilisation_at(now - 2 * DAY, util(0.20, 0.20));
    store.record_utilisation_at(now - 2 * HOUR, util(0.30, 0.30));
    store.record_utilisation_at(now - MINUTE, util(0.40, 0.40));

    let last_hour = store.utilisation_window(now, RetentionWindow::LastHour);
    assert_eq!(
        last_hour.len(),
        1,
        "only the 1-minute-ago sample is within 1h"
    );

    let last_day = store.utilisation_window(now, RetentionWindow::LastDay);
    assert_eq!(last_day.len(), 2, "the 2h and 1m samples are within 24h");

    let last_week = store.utilisation_window(now, RetentionWindow::LastWeek);
    assert_eq!(
        last_week.len(),
        3,
        "the 2d, 2h and 1m samples are within 7d; the 8-day-old one is pruned"
    );
}

#[test]
fn utilisation_percentiles_summarise_a_window() {
    let store = RetentionStore::new();
    let now = 500 * DAY;
    // Ten samples within the last hour at cpu = 0.0, 0.1, .. 0.9.
    for i in 0..10u64 {
        let cpu = f64_tenths(i);
        store.record_utilisation_at(now - (i + 1), util(cpu, cpu));
    }
    let summary = store
        .utilisation_summary(now, RetentionWindow::LastHour)
        .expect("a non-empty window summarises");
    assert_eq!(summary.samples, 10);
    // p50 of {0.0..0.9} is at or above the midpoint; p100 is the max (0.9).
    assert!(
        summary.cpu_p50 >= 0.4,
        "p50 cpu >= 0.4, got {}",
        summary.cpu_p50
    );
    assert!(
        (summary.cpu_p100 - 0.9).abs() < 1e-6,
        "p100 cpu is the max 0.9, got {}",
        summary.cpu_p100
    );
    assert!(
        summary.cpu_p0 <= 0.05,
        "p0 cpu ~ 0.0, got {}",
        summary.cpu_p0
    );
}

#[test]
fn drop_oldest_when_writing_past_capacity() {
    let store = RetentionStore::new();
    let base = 2_000 * DAY;
    // Write one utilisation sample per minute for RETENTION_BUCKETS + 100 minutes.
    // The first 100 minutes must be evicted (drop-oldest); the buffer stays bounded.
    let total = buckets_u64() + 100;
    for minute in 0..total {
        store.record_utilisation_at(base + minute * MINUTE, util(0.5, 0.5));
    }
    let now = base + (total - 1) * MINUTE;
    // The whole 7d window holds at most RETENTION_BUCKETS samples — never more,
    // proving bounded memory + drop-oldest.
    let week = store.utilisation_window(now, RetentionWindow::LastWeek);
    assert!(
        week.len() <= RETENTION_BUCKETS,
        "the window never exceeds capacity: {} <= {}",
        week.len(),
        RETENTION_BUCKETS
    );
    // The oldest 100 minutes are gone: a query anchored at the very first minute
    // (now well outside the 7d window) returns nothing from that era.
    let old_anchor = base + 50 * MINUTE;
    let stale = store.utilisation_window(old_anchor, RetentionWindow::LastHour);
    assert!(
        stale.is_empty(),
        "evicted early minutes must not resurface, got {} samples",
        stale.len()
    );
}

#[test]
fn reconnect_history_is_recorded_and_windowed() {
    let store = RetentionStore::new();
    let now = 700 * DAY;
    store.record_reconnect_at(now - 3 * DAY, "cam-1", 1);
    store.record_reconnect_at(now - 2 * HOUR, "cam-1", 2);
    store.record_reconnect_at(now - 30, "cam-2", 1);

    let week = store.reconnect_window(now, RetentionWindow::LastWeek);
    assert_eq!(week.len(), 3, "all three reconnects fall within 7d");

    let hour = store.reconnect_window(now, RetentionWindow::LastHour);
    assert_eq!(
        hour.len(),
        1,
        "only cam-2's reconnect is within the last hour"
    );
    assert_eq!(hour[0].input_id, "cam-2");
    assert_eq!(hour[0].attempt, 1);
}

#[test]
fn shed_events_are_recorded_and_windowed() {
    let store = RetentionStore::new();
    let now = 900 * DAY;
    store.record_shed_at(now - 5 * DAY, ShedReason::Pinned);
    store.record_shed_at(now - 10, ShedReason::AntiStorm);

    let week = store.shed_window(now, RetentionWindow::LastWeek);
    assert_eq!(week.len(), 2);
    let hour = store.shed_window(now, RetentionWindow::LastHour);
    assert_eq!(hour.len(), 1);
    assert_eq!(hour[0].reason, ShedReason::AntiStorm);
}

#[test]
fn shed_reason_labels_cover_every_variant() {
    // Every reason has a stable, distinct lower-case label (the store mirrors the
    // engine's `ShedReason` + the egress encoder-overload shed).
    assert_eq!(ShedReason::Pinned.label(), "pinned");
    assert_eq!(ShedReason::DisplayBound.label(), "display_bound");
    assert_eq!(ShedReason::NoBetterHome.label(), "no_better_home");
    assert_eq!(ShedReason::AntiStorm.label(), "anti_storm");
    assert_eq!(ShedReason::EncoderOverload.label(), "encoder_overload");
}

#[test]
fn display_bound_and_encoder_overload_sheds_are_recorded() {
    let store = RetentionStore::new();
    let now = 950 * DAY;
    store.record_shed_at(now - 30, ShedReason::DisplayBound);
    store.record_shed_at(now - 10, ShedReason::EncoderOverload);
    let hour = store.shed_window(now, RetentionWindow::LastHour);
    assert_eq!(hour.len(), 2);
    assert_eq!(hour[0].reason, ShedReason::DisplayBound);
    assert_eq!(hour[1].reason, ShedReason::EncoderOverload);
}

#[test]
fn incident_markers_carry_kind_and_timestamp() {
    let store = RetentionStore::new();
    let now = 1_100 * DAY;
    store.record_incident_at(now - 2 * DAY, IncidentKind::InputFlap, "cam-3");
    store.record_incident_at(now - 90, IncidentKind::EncoderSaturation, "program");
    store.record_incident_at(now - 30, IncidentKind::ClockHoldover, "system");

    let week = store.incident_window(now, RetentionWindow::LastWeek);
    assert_eq!(week.len(), 3);
    let hour = store.incident_window(now, RetentionWindow::LastHour);
    assert_eq!(
        hour.len(),
        2,
        "the 90s and 30s incidents are within the hour"
    );
    // Most recent first ordering is not required; assert the set of kinds present.
    let kinds: Vec<IncidentKind> = hour.iter().map(|i| i.kind).collect();
    assert!(kinds.contains(&IncidentKind::EncoderSaturation));
    assert!(kinds.contains(&IncidentKind::ClockHoldover));
}

#[test]
fn writers_never_block_under_concurrency() {
    // A property of invariant #10: several writers hammering the store concurrently
    // must all make progress and finish; none may deadlock or be starved by another
    // writer or by a concurrent reader. We bound the test with a watchdog thread.
    let store = Arc::new(RetentionStore::new());
    let done = Arc::new(AtomicBool::new(false));

    let watchdog_done = Arc::clone(&done);
    let watchdog = thread::spawn(move || {
        // Generous bound: if the writers were blocking, this fires first.
        for _ in 0..200 {
            if watchdog_done.load(Ordering::Relaxed) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    });

    let base = 3_000 * DAY;
    let mut writers = Vec::new();
    for w in 0..8u64 {
        let store = Arc::clone(&store);
        writers.push(thread::spawn(move || {
            for i in 0..5_000u64 {
                let ts = base + (w * 5_000 + i) % buckets_u64() * MINUTE;
                store.record_utilisation_at(ts, util(0.5, 0.5));
                store.record_reconnect_at(ts, "cam", 1);
                store.record_shed_at(ts, ShedReason::NoBetterHome);
                store.record_incident_at(ts, IncidentKind::InputFlap, "cam");
            }
        }));
    }
    // A concurrent reader, racing the writers, must not block them either.
    let reader_store = Arc::clone(&store);
    let reader = thread::spawn(move || {
        let now = base + buckets_u64() * MINUTE;
        for _ in 0..1_000 {
            let _ = reader_store.utilisation_window(now, RetentionWindow::LastWeek);
            let _ = reader_store.reconnect_window(now, RetentionWindow::LastDay);
        }
    });

    for h in writers {
        h.join()
            .expect("a writer thread must finish (never block forever)");
    }
    reader.join().expect("the reader thread must finish");
    done.store(true, Ordering::Relaxed);
    let watchdog_ok = watchdog.join().expect("watchdog joins");
    assert!(
        watchdog_ok,
        "writers did not finish within the watchdog budget — a writer blocked"
    );
}

#[test]
fn store_has_no_consent_input_anywhere() {
    // CONSPECT/ADR-0052 §3: local retention is consent-INDEPENDENT. The store must
    // record regardless of any telemetry-consent state, because there is no consent
    // parameter on the recording path at all. We assert this structurally: recording
    // succeeds with no consent argument and the data is queryable. (A consent gate
    // would force a bool argument here and this test would not compile.)
    let store = RetentionStore::new();
    let now = 42 * DAY;
    store.record_utilisation_at(now - 5, util(0.7, 0.7));
    let window = store.utilisation_window(now, RetentionWindow::LastHour);
    assert_eq!(
        window.len(),
        1,
        "retention records with no consent check — it is consent-independent"
    );
}

/// Build a utilisation sample with the given cpu + gpu busy fractions.
fn util(cpu: f64, gpu: f64) -> multiview_telemetry::retention::UtilisationSample {
    multiview_telemetry::retention::UtilisationSample {
        cpu_util: cpu,
        gpu_util: Some(gpu),
        program_fps: None,
    }
}

/// `i / 10` as an `f64` without a lossy cast (i is small, < 10).
fn f64_tenths(i: u64) -> f64 {
    let n = u32::try_from(i).unwrap_or(0);
    f64::from(n) / 10.0
}
