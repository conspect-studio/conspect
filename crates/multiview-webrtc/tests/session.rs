//! Failing-first tests for the session map: >=128-bit random ids, the
//! preview/output viewer-pool cap (ingest/push admitted outside it), idle GC, and
//! 60 s tombstone eviction (ADR-0048 §8).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use std::time::{Duration, Instant};

use multiview_webrtc::session::{SessionId, SessionRole, SessionTable};

#[test]
fn session_ids_are_random_at_least_128_bit_and_distinct() {
    let a = SessionId::random();
    let b = SessionId::random();
    assert_ne!(a, b);
    // base64url of >=16 bytes is >=22 chars.
    assert!(a.as_str().len() >= 22, "id too short: {}", a.as_str());
    // No `+`/`/`/`=` — url-safe alphabet only.
    assert!(a
        .as_str()
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
}

#[test]
fn viewer_pool_cap_applies_to_preview_and_output_viewers_only() {
    let mut table = SessionTable::new(2, Duration::from_secs(30), Duration::from_secs(60));
    let now = Instant::now();
    // Two viewer sessions fill the pool.
    let v1 = table.admit(SessionRole::PreviewViewer, now).unwrap();
    let _v2 = table.admit(SessionRole::OutputViewer, now).unwrap();
    // A third viewer is refused (at capacity).
    assert!(table.admit(SessionRole::OutputViewer, now).is_err());
    // But a WHIP ingest publisher and a whip_push client are admitted OUTSIDE the
    // pool — a viewer flood never starves a publisher (ADR-0048 §8).
    assert!(table.admit(SessionRole::IngestPublisher, now).is_ok());
    assert!(table.admit(SessionRole::PushClient, now).is_ok());
    // Closing a viewer frees a pool slot.
    table.close(&v1, now);
    assert!(table.admit(SessionRole::PreviewViewer, now).is_ok());
}

#[test]
fn idle_session_is_garbage_collected_and_becomes_a_tombstone() {
    let mut table = SessionTable::new(8, Duration::from_secs(30), Duration::from_secs(60));
    let now = Instant::now();
    let id = table.admit(SessionRole::PreviewViewer, now).unwrap();
    // No activity for longer than the idle timeout → GC closes it.
    let later = now + Duration::from_secs(31);
    table.gc(later);
    assert!(table.is_closed(&id), "idle session is closed");
    // A DELETE inside the tombstone window is still idempotently known.
    assert!(table.is_known(&id));
}

#[test]
fn activity_defers_idle_gc() {
    let mut table = SessionTable::new(8, Duration::from_secs(30), Duration::from_secs(60));
    let now = Instant::now();
    let id = table.admit(SessionRole::IngestPublisher, now).unwrap();
    // Touch the session just before the timeout.
    table.touch(&id, now + Duration::from_secs(25));
    table.gc(now + Duration::from_secs(31));
    assert!(!table.is_closed(&id), "recent activity defers GC");
}

#[test]
fn tombstone_is_evicted_after_its_ttl() {
    let mut table = SessionTable::new(8, Duration::from_secs(30), Duration::from_secs(60));
    let now = Instant::now();
    let id = table.admit(SessionRole::OutputViewer, now).unwrap();
    table.close(&id, now);
    assert!(table.is_known(&id), "freshly closed id is a live tombstone");
    // After the 60 s tombstone TTL, the entry is evicted — bounded memory.
    table.gc(now + Duration::from_secs(61));
    assert!(!table.is_known(&id), "tombstone evicted after its TTL");
}

#[test]
fn closing_a_viewer_frees_the_pool_but_close_is_idempotent() {
    let mut table = SessionTable::new(1, Duration::from_secs(30), Duration::from_secs(60));
    let now = Instant::now();
    let id = table.admit(SessionRole::PreviewViewer, now).unwrap();
    table.close(&id, now);
    // Double close is a no-op, not a panic, and does not under-count the pool.
    table.close(&id, now);
    assert!(table.admit(SessionRole::PreviewViewer, now).is_ok());
}
