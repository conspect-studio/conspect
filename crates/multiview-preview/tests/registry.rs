//! Tap registry: refcounting, lazy-start on first subscriber, auto-stop on last
//! leave, and the ISOLATION property — a slow/absent preview consumer never
//! back-pressures the source.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use multiview_engine::isolation::EventStream;
use multiview_preview::{TapKey, TapRegistry, TapScope};

/// A factory that counts how many times a tap was started and stopped, so the
/// tests can assert lazy-start and auto-stop precisely. Each start hands out a
/// fresh broadcast subscription onto a shared upstream `EventStream<u64>`.
#[derive(Clone)]
struct CountingSource {
    upstream: EventStream<u64>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl CountingSource {
    fn new(upstream: EventStream<u64>) -> Self {
        Self {
            upstream,
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }
    }
}

fn key() -> TapKey {
    TapKey::new(TapScope::Program, "program")
}

#[tokio::test]
async fn lazy_start_only_on_first_subscriber() {
    let (upstream, _seed) = multiview_engine::isolation::event_stream::<u64>(8);
    let src = CountingSource::new(upstream);
    let starts = Arc::clone(&src.starts);
    let stops = Arc::clone(&src.stops);

    let registry: TapRegistry<u64> = TapRegistry::new();
    assert_eq!(
        starts.load(Ordering::SeqCst),
        0,
        "no tap before any subscriber"
    );

    let starts_cb = Arc::clone(&starts);
    let stops_cb = Arc::clone(&stops);
    let upstream = src.upstream.clone();
    let lease1 = registry
        .subscribe(key(), move || {
            starts_cb.fetch_add(1, Ordering::SeqCst);
            let up = upstream.clone();
            let stop = Arc::clone(&stops_cb);
            (up.subscribe(), move || {
                stop.fetch_add(1, Ordering::SeqCst);
            })
        })
        .expect("first subscribe starts the tap");
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "first subscriber starts the tap"
    );
    assert_eq!(registry.subscriber_count(&key()), 1);

    // A second subscriber MUST reuse the running tap — no second start. The
    // factory has a concrete return type but must never be invoked; if it is,
    // the panic fails the test.
    let lease2 = registry
        .subscribe(
            key(),
            || -> (multiview_engine::isolation::EventSubscription<u64>, fn()) {
                panic!("must not start a second tap")
            },
        )
        .expect("second subscribe reuses the tap");
    assert_eq!(starts.load(Ordering::SeqCst), 1, "shared tap, fan-out many");
    assert_eq!(registry.subscriber_count(&key()), 2);

    // Dropping one lease keeps the tap alive (refcount 2 -> 1).
    drop(lease1);
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "tap stays up while a subscriber remains"
    );
    assert_eq!(registry.subscriber_count(&key()), 1);

    // Dropping the last lease auto-stops the tap (refcount 1 -> 0).
    drop(lease2);
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "last leave auto-stops the tap"
    );
    assert_eq!(registry.subscriber_count(&key()), 0);
}

#[tokio::test]
async fn lazy_restart_after_full_teardown() {
    let (upstream, _seed) = multiview_engine::isolation::event_stream::<u64>(8);
    let src = CountingSource::new(upstream);
    let starts = Arc::clone(&src.starts);
    let stops = Arc::clone(&src.stops);
    let registry: TapRegistry<u64> = TapRegistry::new();

    let mk = || {
        let starts = Arc::clone(&starts);
        let stops = Arc::clone(&stops);
        let upstream = src.upstream.clone();
        move || {
            starts.fetch_add(1, Ordering::SeqCst);
            let up = upstream.clone();
            let stop = Arc::clone(&stops);
            (up.subscribe(), move || {
                stop.fetch_add(1, Ordering::SeqCst);
            })
        }
    };

    let lease = registry.subscribe(key(), mk()).unwrap();
    drop(lease);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(registry.subscriber_count(&key()), 0);

    // A fresh subscriber after teardown lazily starts the tap AGAIN.
    let lease = registry.subscribe(key(), mk()).unwrap();
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "tap restarts on a new first subscriber"
    );
    drop(lease);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn distinct_keys_get_distinct_taps() {
    let (upstream, _seed) = multiview_engine::isolation::event_stream::<u64>(8);
    let src = CountingSource::new(upstream);
    let starts = Arc::clone(&src.starts);
    let registry: TapRegistry<u64> = TapRegistry::new();

    let mk = || {
        let starts = Arc::clone(&starts);
        let upstream = src.upstream.clone();
        move || {
            starts.fetch_add(1, Ordering::SeqCst);
            let up = upstream.clone();
            (up.subscribe(), || {})
        }
    };

    let _a = registry
        .subscribe(TapKey::new(TapScope::Input, "a"), mk())
        .unwrap();
    let _b = registry
        .subscribe(TapKey::new(TapScope::Input, "b"), mk())
        .unwrap();
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "two distinct keys => two taps"
    );
    assert_eq!(registry.active_taps(), 2);
}

#[tokio::test]
async fn lease_delivers_frames_from_upstream() {
    let (upstream, _seed) = multiview_engine::isolation::event_stream::<u64>(16);
    let registry: TapRegistry<u64> = TapRegistry::new();

    let up = upstream.clone();
    let mut lease = registry
        .subscribe(key(), move || (up.subscribe(), || {}))
        .unwrap();

    upstream.publish(11);
    upstream.publish(22);

    let first = lease.recv().await.expect("first frame");
    assert_eq!(*first.event, 11);
    let second = lease.recv().await.expect("second frame");
    assert_eq!(*second.event, 22);
}

/// ISOLATION (invariant #10): a preview lease that NEVER reads must not slow,
/// stall, or grow the upstream publisher. The publisher is bounded drop-oldest;
/// publishing stays wait-free regardless of the dead consumer, and the live
/// consumer keeps making progress (observing a `Lagged` skip, never a hang).
#[tokio::test]
async fn slow_consumer_never_backpressures_source() {
    let (upstream, _seed) = multiview_engine::isolation::event_stream::<u64>(4);
    let registry: TapRegistry<u64> = TapRegistry::new();

    // A dead lease that we never read from.
    let up = upstream.clone();
    let _dead = registry
        .subscribe(TapKey::new(TapScope::Program, "dead"), move || {
            (up.subscribe(), || {})
        })
        .unwrap();

    // A live lease that we DO read from.
    let up = upstream.clone();
    let mut live = registry
        .subscribe(TapKey::new(TapScope::Program, "live"), move || {
            (up.subscribe(), || {})
        })
        .unwrap();

    // Publish far more than the ring depth. Each publish must return promptly;
    // a back-pressuring channel would block here. We bound the whole thing in a
    // timeout so a regression manifests as a test FAILURE, not a hang.
    let published = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for i in 0..10_000u64 {
            // `publish` is wait-free and returns the assigned seq.
            let _ = upstream.publish(i);
        }
        true
    })
    .await
    .expect("publishing must never block on a slow/absent consumer");
    assert!(published);

    // The live consumer still makes progress: it sees a Lagged skip (drop-oldest)
    // and then continues reading the newest buffered frames — it never deadlocks.
    // We drain only what is currently buffered (`try_recv`, no await), so the
    // test is deterministic and cannot hang on an open-but-empty channel.
    let mut saw_lag = false;
    let mut saw_frame = false;
    loop {
        match live.try_recv() {
            Ok(seq) => {
                saw_frame = true;
                assert!(*seq.event < 10_000);
            }
            Err(multiview_engine::isolation::TryRecvError::Lagged(n)) => {
                saw_lag = true;
                assert!(n > 0);
            }
            // Nothing more buffered for this viewer, or the publisher is gone:
            // either way the consumer has finished making the progress it can.
            Err(
                multiview_engine::isolation::TryRecvError::Empty
                | multiview_engine::isolation::TryRecvError::Closed,
            ) => break,
        }
    }
    assert!(
        saw_lag,
        "a slow consumer over a drop-oldest ring must observe Lagged"
    );
    assert!(
        saw_frame,
        "after lagging, the consumer resumes reading newest frames"
    );
}
