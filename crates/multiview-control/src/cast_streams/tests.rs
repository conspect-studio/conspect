#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unchecked_time_subtraction
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::*;
use crate::output_routing::RoutedFeed;

fn clocked() -> (Arc<AtomicU64>, CastStreams) {
    let now = Arc::new(AtomicU64::new(1_000_000));
    let c = Arc::clone(&now);
    (
        now,
        CastStreams::with_clock(Arc::new(move || c.load(Ordering::SeqCst))),
    )
}

fn advance(now: &AtomicU64, d: Duration) {
    now.fetch_add(u64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
}

#[test]
fn registry_holds_no_plaintext_token() {
    let (_, streams) = clocked();
    let claim = streams
        .claim("op", "output-2", "c1", &RoutedFeed::Program, false)
        .unwrap();
    let dump = format!("{:?}", streams.lock());
    assert!(!dump.contains(&claim.token), "{dump}");
    assert_eq!(streams.lookup(&claim.token).unwrap().1, "c1");
    // The owner's view re-derives the same token.
    assert_eq!(streams.get(&claim.id).unwrap().1, claim.token);
}

#[test]
fn pending_idle_and_absolute_expiry() {
    let (now, streams) = clocked();
    let a = streams
        .claim("op", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    advance(&now, PENDING_TTL - Duration::from_millis(1));
    assert!(streams.lookup(&a.token).is_some());
    advance(&now, Duration::from_millis(1));
    assert!(streams.lookup(&a.token).is_none(), "pending expired");

    let b = streams
        .claim("op", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    // Live: each playlist fetch pushes the idle expiry out.
    for _ in 0..20 {
        advance(&now, IDLE_TTL - Duration::from_secs(1));
        assert!(streams.lookup(&b.token).is_some());
        streams.record_fetch(&b.id, true, Some("Mozilla CrKey/1.56"));
    }
    advance(&now, IDLE_TTL);
    assert!(streams.lookup(&b.token).is_none(), "idle expired");

    let c = streams
        .claim("op", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    let mut elapsed = Duration::ZERO;
    while elapsed < MAX_LIFETIME - Duration::from_secs(30) {
        advance(&now, Duration::from_secs(30));
        elapsed += Duration::from_secs(30);
        streams.record_fetch(&c.id, true, None);
    }
    advance(&now, Duration::from_secs(30));
    assert!(streams.lookup(&c.token).is_none(), "absolute max");
}

#[test]
fn caps_total_and_per_principal() {
    let (_, streams) = clocked();
    for i in 0..MAX_STREAMS_PER_PRINCIPAL {
        streams
            .claim("p", &format!("o{i}"), "c", &RoutedFeed::Program, false)
            .unwrap();
    }
    assert_eq!(
        streams
            .claim("p", "o", "c", &RoutedFeed::Program, false)
            .unwrap_err(),
        ClaimError::PrincipalCapacity
    );
    for i in MAX_STREAMS_PER_PRINCIPAL..MAX_STREAMS {
        streams
            .claim(&format!("q{i}"), "o", "c", &RoutedFeed::Program, false)
            .unwrap();
    }
    assert_eq!(
        streams
            .claim("z", "o", "c", &RoutedFeed::Program, false)
            .unwrap_err(),
        ClaimError::Capacity
    );
}

#[test]
fn first_shared_then_linger_and_end() {
    let (now, streams) = clocked();
    let a = streams
        .claim("p", "o", "c1", &RoutedFeed::Source("s".into()), false)
        .unwrap();
    assert!(a.first);
    let b = streams
        .claim("q", "o", "c1", &RoutedFeed::Source("s".into()), false)
        .unwrap();
    assert!(!b.first);
    streams.mark_moved("c1");
    // Another feed on a used output with expect_free loses.
    assert_eq!(
        streams
            .claim("q", "o", "c1", &RoutedFeed::Program, true)
            .unwrap_err(),
        ClaimError::Taken
    );
    streams.remove(&a.id).unwrap();
    assert_eq!(streams.sweep(), Vec::<Ended>::new());
    streams.remove(&b.id).unwrap();
    assert!(streams.lookup(&b.token).is_none(), "revoked at once");
    assert_eq!(streams.sweep(), Vec::<Ended>::new(), "linger starts");
    advance(&now, LINGER - Duration::from_millis(1));
    assert_eq!(streams.sweep(), Vec::<Ended>::new());
    advance(&now, Duration::from_millis(1));
    let ended = streams.sweep();
    assert_eq!(
        ended,
        vec![Ended {
            output: "o".into(),
            config_id: "c1".into(),
            moved: true
        }]
    );
    assert_eq!(streams.sizes().1, 0);
}

#[test]
fn reports_are_rate_limited_and_stored() {
    let (now, streams) = clocked();
    let a = streams
        .claim("p", "o", "c1", &RoutedFeed::Program, false)
        .unwrap();
    streams
        .report(&a.token, serde_json::json!({"state": "playing"}))
        .unwrap();
    assert_eq!(
        streams.report(&a.token, serde_json::json!({})).unwrap_err(),
        ReportError::TooSoon
    );
    advance(&now, REPORT_INTERVAL);
    streams
        .report(&a.token, serde_json::json!({"state": "buffering"}))
        .unwrap();
    let view = streams.get(&a.id).unwrap().0;
    assert_eq!(view.last_report.unwrap().report["state"], "buffering");
    assert_eq!(
        streams.report("x".repeat(43).as_str(), serde_json::json!({})),
        Err(ReportError::Unknown)
    );
}

#[test]
fn create_limiter_burst_then_one_per_second() {
    let (now, streams) = clocked();
    for _ in 0..CREATE_BURST {
        streams.admit_create("p").unwrap();
    }
    assert!(streams.admit_create("p").is_err());
    assert!(streams.admit_create("other").is_ok());
    advance(&now, CREATE_REFILL);
    streams.admit_create("p").unwrap();
    assert!(streams.admit_create("p").is_err());
}

#[test]
fn memory_stays_bounded_under_a_create_expire_storm() {
    let (now, streams) = clocked();
    for round in 0..200u32 {
        for i in 0..MAX_STREAMS {
            let _ = streams.claim(
                &format!("p{round}-{i}"),
                "o",
                &format!("c{}", i % 4),
                &RoutedFeed::Program,
                false,
            );
            let _ = streams.admit_create(&format!("p{round}-{i}"));
        }
        advance(&now, PENDING_TTL + LINGER);
        let _ = streams.sweep();
        let (entries, slots, limited, rates) = streams.sizes();
        assert!(entries <= MAX_STREAMS && slots <= 4);
        assert!(limited <= MAX_LIMITED_PRINCIPALS && rates <= MAX_STREAMS);
    }
}

/// INV (round 3): gate creation is bounded by [`MAX_GATES`] and fails
/// closed: past it no gate is created, nothing is cleared and nothing can
/// be armed or served on that output.
#[test]
fn gates_are_bounded_and_fail_closed_past_the_cap() {
    let (_, streams) = clocked();
    let claim = streams
        .claim("op", "output-x", "over", &RoutedFeed::Program, false)
        .unwrap();
    for i in 0..MAX_GATES {
        let arm = streams.arm_snapshot(&format!("c{i}"));
        assert!(arm.generation > 0);
    }
    let arm = streams.arm_snapshot("over");
    assert_eq!(arm.generation, 0);
    assert!(arm.needs.is_none());
    assert_eq!(streams.lock().gates.len(), MAX_GATES);
    assert_eq!(
        streams.arm(&claim.id, arm.generation, None),
        Err(api::ArmError::Stale)
    );
    assert!(streams.serve(&claim.token).is_none());
    assert_eq!(streams.generation("over"), None);
}
