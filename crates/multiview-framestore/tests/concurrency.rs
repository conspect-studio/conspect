#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Concurrency tests: a publisher thread races a reader thread and the reader
//! must always observe a valid, internally-consistent, never-older-than-seen
//! value — no tearing, no stalls, no regressions in the published sequence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use multiview_core::time::MediaTime;
use multiview_framestore::{LatestSlot, TileStore};

/// A payload whose two fields are derived from the same counter, so a *torn*
/// read (mixing the high half of one publish with the low half of another)
/// would be detectable. With `Arc`-based publishing tearing is impossible by
/// construction; this asserts that guarantee empirically under contention.
#[derive(Clone, Copy, Debug)]
struct Tagged {
    /// The monotonically increasing counter value.
    counter: u64,
    /// A redundant copy that must always equal `counter` (the consistency
    /// check — a torn read would break this).
    mirror: u64,
}

#[test]
fn reader_never_sees_torn_value() {
    const PUBLISHES: u64 = 200_000;

    let slot: Arc<LatestSlot<Tagged>> = Arc::new(LatestSlot::new());
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let slot = Arc::clone(&slot);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            for counter in 1..=PUBLISHES {
                slot.publish(Tagged {
                    counter,
                    mirror: counter,
                });
            }
            stop.store(true, Ordering::Release);
        })
    };

    let reader = {
        let slot = Arc::clone(&slot);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut reads: u64 = 0;
            let mut max_seen: u64 = 0;
            loop {
                if let Some(v) = slot.load() {
                    // No tearing: the two halves always come from one publish.
                    assert_eq!(
                        v.counter, v.mirror,
                        "torn read detected: {} != {}",
                        v.counter, v.mirror
                    );
                    reads += 1;
                    max_seen = max_seen.max(v.counter);
                }
                if stop.load(Ordering::Acquire) && slot.load().map(|v| v.counter) == Some(PUBLISHES)
                {
                    break;
                }
            }
            (reads, max_seen)
        })
    };

    writer.join().expect("writer thread panicked");
    let (reads, max_seen) = reader.join().expect("reader thread panicked");

    // The reader ran (it is not blocked) and never observed a value beyond what
    // was published.
    assert!(reads > 0, "reader observed no values");
    assert!(
        max_seen <= PUBLISHES,
        "reader saw a value beyond the last publish: {max_seen} > {PUBLISHES}"
    );
    // The final published value is observable.
    assert_eq!(slot.load().map(|v| v.counter), Some(PUBLISHES));
}

#[test]
fn reader_never_observes_a_regression_in_sequence() {
    // A single reader observing one writer must see a non-decreasing sequence
    // of counter values (newest-wins; the slot never hands back an older
    // value than one it already returned to this same reader).
    //
    // This is guaranteed by read-read coherence over the single atomic pointer
    // backing the slot (C++/Rust [intro.races]/4 + /12): a thread's sequenced
    // loads of one atomic location can never move backwards in that location's
    // single total modification order — on x86 *or* ARM/AArch64, at any atomic
    // ordering. So `*v >= prev` below must hold on every iteration on every
    // platform; a failure here would be a real arc-swap/std bug, never a flake.
    // See the "Ordering guarantee" docs on `LatestSlot`.
    const PUBLISHES: u64 = 300_000;

    let slot: Arc<LatestSlot<u64>> = Arc::new(LatestSlot::new());

    let writer = {
        let slot = Arc::clone(&slot);
        thread::spawn(move || {
            for counter in 1..=PUBLISHES {
                slot.publish(counter);
            }
        })
    };

    let reader = {
        let slot = Arc::clone(&slot);
        thread::spawn(move || {
            let mut prev: u64 = 0;
            let mut samples: u64 = 0;
            // Sample until we have seen the final value.
            loop {
                if let Some(v) = slot.load() {
                    assert!(
                        *v >= prev,
                        "sequence regressed for this reader: {} < {}",
                        *v,
                        prev
                    );
                    prev = *v;
                    samples += 1;
                    if *v == PUBLISHES {
                        break;
                    }
                }
            }
            samples
        })
    };

    writer.join().expect("writer thread panicked");
    let samples = reader.join().expect("reader thread panicked");
    // The reader actually ran (it observed at least one value and so executed
    // the per-iteration `*v >= prev` non-regression check), and it terminated
    // only by observing the final published value — not by any weaker path.
    assert!(samples > 0, "reader observed no values");
    assert_eq!(
        slot.load().map(|v| *v),
        Some(PUBLISHES),
        "final published value must remain observable after the writer finishes"
    );
}

#[test]
fn tile_store_read_is_consistent_under_concurrent_publishing() {
    // The full TileStore (slot + state machine) must keep handing the reader a
    // valid frame while a writer publishes concurrently. The reader injects an
    // advancing clock; because the writer keeps the frame fresh, reads should
    // be Fresh/Held (never spuriously NoSignal once a frame exists) and the
    // payload is always one we actually published.
    const PUBLISHES: u64 = 100_000;

    let store: Arc<TileStore<Tagged>> = Arc::new(TileStore::with_defaults("cam"));

    let writer = {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            for counter in 1..=PUBLISHES {
                // Publish at a time that advances with the counter (1 ms each
                // in ns) so the tile stays well within the default hold window
                // relative to a reader sampling the same clock.
                let at = MediaTime::from_nanos(i64::try_from(counter).unwrap() * 1_000_000);
                store.publish(
                    Tagged {
                        counter,
                        mirror: counter,
                    },
                    at,
                );
            }
        })
    };

    let reader = {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            let mut observed: u64 = 0;
            loop {
                let seq = store.sequence();
                // Read at "now" tracking the writer's clock plus a small margin.
                let now = MediaTime::from_nanos(i64::try_from(seq.max(1)).unwrap() * 1_000_000);
                if let Some(frame) = store.read(now).frame() {
                    // No tearing: the frame's two halves always come from one
                    // published `Tagged` (counter == mirror by construction).
                    assert_eq!(
                        frame.counter, frame.mirror,
                        "torn frame in TileStore: {} != {}",
                        frame.counter, frame.mirror
                    );
                    assert!(frame.counter <= PUBLISHES);
                    observed = observed.max(frame.counter);
                }
                if seq >= PUBLISHES {
                    break;
                }
            }
            observed
        })
    };

    writer.join().expect("writer thread panicked");
    let observed = reader.join().expect("reader thread panicked");
    assert!(observed > 0, "reader observed no frames");
    assert_eq!(store.sequence(), PUBLISHES);
}
