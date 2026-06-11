//! Isolation chaos tests — invariant #10 (the engine can never be
//! back-pressured by a client).
//!
//! THE chaos property: a subscriber that never reads (or reads arbitrarily
//! slowly, or crashes, or holds any lock it can reach) cannot slow the engine's
//! publish path. We assert that publishing to both isolation channels — the
//! wait-free latest-state slot and the drop-oldest event broadcast — completes
//! in bounded time regardless of consumer behaviour, that a lagging subscriber
//! observes drop-oldest (`Lagged`) rather than ever stalling the publisher, and
//! that every event carries a strictly-increasing sequence number for resume.
//!
//! The previous implementation guarded a `VecDeque` ring with a `std::sync::Mutex`
//! shared between `publish()` and the consumers' `try_recv()`/`drain()`/`pending()`;
//! a consumer holding that lock back-pressured the engine. These tests now run
//! against the lock-free / `broadcast`-based replacement, which has no such lock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_engine::{event_stream, EnginePublisher, EventStream, LatestState, TryRecvError};

#[test]
fn latest_state_publish_is_wait_free_and_newest_wins() {
    let state = LatestState::<u64>::new();
    assert!(state.latest().is_none());
    for i in 0..10_000_u64 {
        state.publish(i);
    }
    // A consumer reading "whenever" sees the newest value, never an old one, and
    // the publisher (a single atomic store) never waited on it.
    assert_eq!(*state.latest().unwrap(), 9999);
    assert_eq!(state.sequence(), 10_000);
}

#[test]
fn event_stream_publish_never_blocks_with_no_subscribers() {
    // No subscriber at all: `broadcast::send` returns Err(no receivers), which is
    // a normal "nobody listening" state for the engine, NOT a back-pressure
    // point. Publishing must keep returning monotonic sequence numbers.
    let (tx, only): (EventStream<u64>, _) = event_stream(8);
    drop(only); // no live subscribers remain.

    let start = Instant::now();
    let n = 1_000_000_u64;
    let mut last_seq = 0;
    for i in 0..n {
        let seq = tx.publish(i);
        assert_eq!(
            seq,
            last_seq + 1,
            "sequence numbers are strictly increasing"
        );
        last_seq = seq;
    }
    let elapsed = start.elapsed();
    assert_eq!(tx.sequence(), n);
    assert_eq!(tx.subscriber_count(), 0);
    // A million publishes into a no-listener channel is sub-second; the point is
    // it COMPLETES (a blocking channel would deadlock forever).
    assert!(
        elapsed < Duration::from_secs(30),
        "publishing with no subscribers must not block: took {elapsed:?}"
    );
}

#[test]
fn stalled_subscriber_lags_and_never_back_pressures_the_publisher() {
    // A subscriber that NEVER reads. Publishing far past capacity must stay fast
    // and bounded-time; the channel's per-subscriber memory is structurally
    // bounded to `capacity` slots (drop-oldest), and when the laggard finally
    // reads it observes `Lagged` and can only ever drain the retained newest
    // `capacity` events.
    let capacity = 8_usize;
    let (tx, mut never_read): (EventStream<[u8; 256]>, _) = event_stream(capacity);
    let payload = [0xAB_u8; 256];

    let start = Instant::now();
    let n = 1_000_000_u64;
    for _ in 0..n {
        tx.publish(payload);
    }
    let elapsed = start.elapsed();

    assert_eq!(tx.sequence(), n);
    assert_eq!(tx.capacity(), capacity);
    // Keep the "publisher never blocks" property: a million publishes into a
    // never-read subscriber COMPLETES (a blocking channel would deadlock).
    assert!(
        elapsed < Duration::from_secs(30),
        "publishing into a never-read subscriber must not block: took {elapsed:?}"
    );

    // NOTE on measuring bounded memory: `broadcast::Receiver::len()` reports the
    // LAG DISTANCE (messages sent since this receiver's position — here ~= n),
    // NOT the buffered-memory occupancy, so `len() <= capacity` is the wrong
    // measurement and would (correctly) fail. The real invariant — the channel's
    // retained buffer never exceeds `capacity` (drop-oldest) — is asserted
    // directly below via the count of events that remain drainable after overflow.

    // The stalled subscriber fell behind: its first read is a `Lagged` reporting
    // exactly how many it missed. With drop-oldest it retained only the newest
    // `capacity` events, so it skipped `n - capacity`.
    let capacity_u64 = u64::try_from(capacity).unwrap();
    let expected_missed = n - capacity_u64;
    match never_read.try_recv() {
        Err(TryRecvError::Lagged(missed)) => {
            assert_eq!(
                missed, expected_missed,
                "drop-oldest must skip everything except the retained newest `capacity`"
            );
        }
        other => panic!("expected Lagged after overflow, got {other:?}"),
    }

    // Bounded memory, asserted directly: after the Lagged resync the receiver can
    // drain AT MOST `capacity` remaining events, in strictly-increasing seq order
    // — proving the retained buffer never exceeded `capacity`.
    let mut drained = 0_usize;
    let mut last_seq = n - capacity_u64;
    loop {
        match never_read.try_recv() {
            Ok(ev) => {
                assert!(
                    ev.seq > last_seq,
                    "retained events must be in strictly-increasing seq order"
                );
                last_seq = ev.seq;
                drained += 1;
            }
            Err(TryRecvError::Empty) => break,
            Err(other) => panic!("unexpected receive error while draining: {other:?}"),
        }
    }
    assert!(
        drained <= capacity,
        "retained buffer ({drained}) must never exceed capacity ({capacity})"
    );
    assert_eq!(
        last_seq, n,
        "the very newest published event must be retained"
    );
}

#[test]
fn drop_oldest_retains_the_newest_capacity_events_in_seq_order() {
    // After overflow, a subscriber resyncs (via Lagged) then drains the NEWEST
    // `capacity` events, in strictly-increasing sequence order.
    let (tx, mut rx): (EventStream<u64>, _) = event_stream(4);
    for i in 0..1000_u64 {
        tx.publish(i);
    }
    // First read trips Lagged (we fell behind); subsequent reads are the newest 4.
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Lagged(_))));

    let mut got = Vec::new();
    let mut last_seq = 0;
    loop {
        match rx.try_recv() {
            Ok(ev) => {
                assert!(ev.seq > last_seq, "seq must strictly increase on resume");
                last_seq = ev.seq;
                got.push(*ev.event);
            }
            Err(TryRecvError::Empty) => break,
            Err(other) => panic!("unexpected receive error: {other:?}"),
        }
    }
    assert_eq!(got, vec![996, 997, 998, 999], "newest `capacity` retained");
}

#[test]
fn resume_by_seq_detects_gaps() {
    // A reconnecting consumer uses the per-event sequence number to detect a gap
    // (a jump greater than +1) caused by drop-oldest, so it knows it must
    // resynchronize from the latest snapshot.
    let (tx, mut rx): (EventStream<u64>, _) = event_stream(2);
    tx.publish(10);
    let first = rx.try_recv().unwrap();
    assert_eq!(first.seq, 1);

    // Burst past capacity while the consumer is away.
    for v in 0..100_u64 {
        tx.publish(v);
    }
    // The consumer comes back: it observes Lagged (the explicit gap signal).
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Lagged(_))));
    // The next delivered event's seq is far beyond first.seq + 1 -> a real gap.
    let resumed = rx.try_recv().unwrap();
    assert!(
        resumed.seq > first.seq + 1,
        "resumed seq {} must reveal the gap after seq {}",
        resumed.seq,
        first.seq
    );
}

#[test]
fn publisher_survives_a_crashed_consumer_thread() {
    // Invariant #10: nothing a client does can stall the engine — including a
    // consumer thread that crashes while holding the broadcast's internal state.
    let (tx, rx): (EventStream<u64>, _) = event_stream(4);
    tx.publish(1);

    let mut rx2 = rx.resubscribe();
    let handle = std::thread::spawn(move || {
        let _ = rx2.try_recv();
        panic!("consumer crashed mid-receive");
    });
    let _ = handle.join(); // the consumer thread is gone (panicked).

    // The publisher is entirely unaffected and keeps publishing/dropping oldest.
    for i in 0..100_u64 {
        tx.publish(i);
    }
    assert_eq!(tx.sequence(), 101);
    drop(rx);
}

#[tokio::test]
async fn async_consumer_task_stall_does_not_block_the_publisher() {
    // A spawned async consumer that just sleeps forever (never drains). The
    // engine-side publisher (on the main task) must complete all its publishes
    // without awaiting the consumer.
    let publisher: EnginePublisher<u64, u64> = EnginePublisher::new(16);
    let stalled = publisher.subscribe();

    let counter = Arc::new(AtomicU64::new(0));
    let c2 = Arc::clone(&counter);
    let consumer = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            // Touch the receiver so it is genuinely held by this task (it still
            // never actually drains), then bump the progress counter.
            let _ = stalled.len();
            c2.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Publisher runs to completion synchronously, no `.await` on the consumer.
    for i in 0..50_000_u64 {
        publisher.publish_state(i);
        publisher.publish_event(i);
    }
    assert_eq!(*publisher.state.latest().unwrap(), 49_999);
    assert_eq!(publisher.events.sequence(), 50_000);
    // The consumer never made progress, proving it never gated the publisher.
    assert_eq!(counter.load(Ordering::Relaxed), 0);

    consumer.abort();
}
