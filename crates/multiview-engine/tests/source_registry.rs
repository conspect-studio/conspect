//! MP-2 acceptance: the process-global [`SourceRegistry`] (decode-once,
//! use-many; ADR-0030 §3, ADR-0034 §2).
//!
//! The registry is the owner of source identity → (one decode actor + one shared
//! [`TileStore`]). Every consumer that resolves to the SAME canonical source key
//! shares ONE decode and holds an [`Arc`] clone of the store; it *samples* the
//! store lock-free (never through the registry lock). The bar:
//!
//! * **(a) one key → one decode.** Two consumers of one canonical key create the
//!   decode actor exactly once and share a single entry.
//! * **(b) shared store.** Both consumers hold [`Arc`] clones of the *same*
//!   [`TileStore`], so a frame the shared decode publishes is visible to both.
//! * **(c) ref-count lifecycle.** The entry is created on the first reference and
//!   torn down on the LAST release — not before.
//! * **(d) chaos / inv #1 + #10.** A wedged source's teardown (a stuck
//!   decode-thread join) must NOT block the releasing consumer's `Drop` (it is a
//!   wait-free hand-off to a reaper thread) and must NOT stall a sibling entry's
//!   lock-free sample path.
//! * **(e) supremum resolution (ADR-0030 §3).** The registry decodes once at the
//!   supremum requested size: the recorded supremum is the per-axis max across all
//!   live references and never shrinks below a larger past request.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_core::time::MediaTime;
use multiview_engine::{RequestedSize, SourceActor, SourceInit, SourceKey, SourceRegistry};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

/// A test ingest/decode actor. Its `shutdown` (the reaper's blocking join)
/// optionally spins on `gate` first — simulating joining a wedged decode thread —
/// then records that teardown ran via `shut`.
struct TestActor {
    shut: Arc<AtomicBool>,
    /// When `Some`, `shutdown` blocks until the gate is cleared (a stuck join).
    gate: Option<Arc<AtomicBool>>,
}

impl SourceActor for TestActor {
    // This acceptance double records only shutdown; it owns no decode thread, so the
    // structural stop signal is a no-op here (the in-crate `ThreadedProbe` unit test proves
    // request_stop actually terminates a real thread on both the graceful and shed paths).
    fn request_stop(&self) {}
    fn shutdown(self: Box<Self>) {
        if let Some(gate) = &self.gate {
            while gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.shut.store(true, Ordering::Release);
    }
}

fn store(id: &str) -> Arc<TileStore<u64>> {
    Arc::new(TileStore::new(
        id,
        TileThresholds::default(),
        NoSignalPolicy::HoldForever,
    ))
}

fn size(width: u32, height: u32) -> RequestedSize {
    RequestedSize { width, height }
}

/// An infallible factory that builds a fresh store + a [`TestActor`], recording
/// its shutdown flag into `shut` and (optionally) wedging its join on `gate`.
fn init(
    id: &'static str,
    shut: Arc<AtomicBool>,
    gate: Option<Arc<AtomicBool>>,
) -> impl FnOnce(RequestedSize) -> Result<SourceInit<u64>, Infallible> {
    move |_req| Ok(SourceInit::new(store(id), TestActor { shut, gate }))
}

fn wait_until(mut cond: impl FnMut() -> bool, within: Duration, what: &str) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < within, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn one_key_creates_exactly_one_decode_entry() {
    let reg = SourceRegistry::<u64>::new();
    let key = SourceKey::from_canonical("rtsp://cam-1/stream");
    let calls = Arc::new(AtomicUsize::new(0));

    let counting = |calls: Arc<AtomicUsize>| {
        move |_req: RequestedSize| {
            calls.fetch_add(1, Ordering::Release);
            Ok::<_, Infallible>(SourceInit::new(
                store("cam-1"),
                TestActor {
                    shut: Arc::new(AtomicBool::new(false)),
                    gate: None,
                },
            ))
        }
    };

    let h1 = reg
        .acquire(key.clone(), size(1280, 720), counting(calls.clone()))
        .unwrap();
    let h2 = reg
        .acquire(key.clone(), size(1280, 720), counting(calls.clone()))
        .unwrap();

    assert_eq!(
        calls.load(Ordering::Acquire),
        1,
        "one canonical key must create exactly ONE decode entry (decode-once)"
    );
    assert_eq!(reg.active_len(), 1);
    assert!(reg.contains(&key));

    drop((h1, h2));
    reg.shutdown();
}

#[test]
fn two_consumers_of_one_key_share_a_single_store() {
    let reg = SourceRegistry::<u64>::new();
    let key = SourceKey::from_canonical("file:///clip.ts#v0");
    let shut = Arc::new(AtomicBool::new(false));

    let h1 = reg
        .acquire(
            key.clone(),
            size(1920, 1080),
            init("clip", shut.clone(), None),
        )
        .unwrap();
    // A second reference to the SAME key must NOT create a new decode.
    let h2 = reg
        .acquire(
            key.clone(),
            size(1920, 1080),
            |_r| -> Result<SourceInit<u64>, Infallible> {
                panic!("second reference to an existing key must NOT run the factory");
            },
        )
        .unwrap();

    assert!(
        Arc::ptr_eq(h1.store(), h2.store()),
        "both consumers must share ONE store (decode-once, use-many)"
    );

    // A frame the shared decode publishes is visible to BOTH consumers.
    h1.store().publish(7_u64, MediaTime::from_nanos(0));
    let r1 = h1.store().read_at(MediaTime::from_nanos(0));
    let r2 = h2.store().read_at(MediaTime::from_nanos(0));
    assert_eq!(r1.frame().map(|f| **f), Some(7));
    assert_eq!(r2.frame().map(|f| **f), Some(7));

    drop((h1, h2));
    reg.shutdown();
}

#[test]
fn entry_created_on_first_reference_and_torn_down_on_last_release() {
    let reg = SourceRegistry::<u64>::new();
    let key = SourceKey::from_canonical("srt://ingest:9000");
    let shut = Arc::new(AtomicBool::new(false));

    let h1 = reg
        .acquire(key.clone(), size(1920, 1080), init("s", shut.clone(), None))
        .unwrap();
    assert!(reg.contains(&key), "entry created on first reference");

    let h2 = reg
        .acquire(
            key.clone(),
            size(1920, 1080),
            |_r| -> Result<SourceInit<u64>, Infallible> {
                panic!("existing key must not re-create")
            },
        )
        .unwrap();

    // Drop ONE handle: the entry (and its decode) MUST survive — a reference remains.
    drop(h1);
    assert!(
        reg.contains(&key),
        "entry must survive while a reference remains"
    );
    assert_eq!(reg.active_len(), 1);
    assert!(
        !shut.load(Ordering::Acquire),
        "the decode must NOT be torn down while a reference remains"
    );

    // Drop the LAST handle: entry removed synchronously; actor handed to the reaper.
    drop(h2);
    assert!(
        !reg.contains(&key),
        "entry must be removed on the LAST release"
    );
    assert_eq!(reg.active_len(), 0);

    // The blocking shutdown runs on the reaper thread; wait (bounded) for it.
    wait_until(
        || shut.load(Ordering::Acquire),
        Duration::from_secs(5),
        "the decode actor must be shut down on last release",
    );

    reg.shutdown();
}

#[test]
fn distinct_keys_get_distinct_decodes() {
    let reg = SourceRegistry::<u64>::new();
    let a = SourceKey::from_canonical("rtsp://a");
    let b = SourceKey::from_canonical("rtsp://b");
    let calls = Arc::new(AtomicUsize::new(0));

    let counting = |calls: Arc<AtomicUsize>| {
        move |_req: RequestedSize| {
            calls.fetch_add(1, Ordering::Release);
            Ok::<_, Infallible>(SourceInit::new(
                store("x"),
                TestActor {
                    shut: Arc::new(AtomicBool::new(false)),
                    gate: None,
                },
            ))
        }
    };

    let ha = reg
        .acquire(a.clone(), size(1, 1), counting(calls.clone()))
        .unwrap();
    let hb = reg
        .acquire(b.clone(), size(1, 1), counting(calls.clone()))
        .unwrap();

    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(reg.active_len(), 2);
    assert!(
        !Arc::ptr_eq(ha.store(), hb.store()),
        "distinct canonical keys must NOT share a store"
    );

    drop((ha, hb));
    reg.shutdown();
}

#[test]
fn decode_supremum_grows_to_the_per_axis_max() {
    // ADR-0030 §3: "Decode once at the supremum requested resolution." The
    // recorded supremum is the per-axis max across all live references and never
    // shrinks below a larger earlier request.
    let reg = SourceRegistry::<u64>::new();
    let key = SourceKey::from_canonical("ndi://STUDIO (CAM 3)");
    let shut = Arc::new(AtomicBool::new(false));

    let h_small = reg
        .acquire(
            key.clone(),
            size(640, 360),
            init("cam3", shut.clone(), None),
        )
        .unwrap();
    assert_eq!(reg.requested_supremum(&key), Some(size(640, 360)));

    // A larger consumer grows the supremum (per-axis max).
    let h_big = reg
        .acquire(
            key.clone(),
            size(1920, 720),
            |_r| -> Result<SourceInit<u64>, Infallible> {
                panic!("existing key must not re-create")
            },
        )
        .unwrap();
    assert_eq!(
        reg.requested_supremum(&key),
        Some(size(1920, 720)),
        "supremum must be the per-axis max of all references"
    );

    // A taller-but-narrower consumer grows only the height axis.
    let h_tall = reg
        .acquire(
            key.clone(),
            size(320, 1080),
            |_r| -> Result<SourceInit<u64>, Infallible> {
                panic!("existing key must not re-create")
            },
        )
        .unwrap();
    assert_eq!(
        reg.requested_supremum(&key),
        Some(size(1920, 1080)),
        "supremum grows independently on each axis"
    );

    // A smaller consumer does NOT shrink the supremum.
    let h_smaller = reg
        .acquire(
            key.clone(),
            size(160, 90),
            |_r| -> Result<SourceInit<u64>, Infallible> {
                panic!("existing key must not re-create")
            },
        )
        .unwrap();
    assert_eq!(reg.requested_supremum(&key), Some(size(1920, 1080)));

    drop((h_small, h_big, h_tall, h_smaller));
    reg.shutdown();
}

#[test]
fn a_wedged_source_teardown_never_stalls_a_sibling_consumer() {
    // CHAOS GATE (inv #1 + #10 for the registry):
    //  (1) the LAST-release `Drop` hands the actor to a reaper via a wait-free,
    //      NON-BLOCKING channel send — it must not block on the actor's join even
    //      when that join is wedged forever;
    //  (2) a sibling entry's lock-free sample path (`Arc<TileStore>::read_at`) keeps
    //      returning frames while a sibling's decode is wedged AND while its teardown
    //      is blocked on the reaper — the registry lock is never held across a join.
    let reg = SourceRegistry::<u64>::new();

    // Entry A: its `shutdown` is WEDGED on a gate (a stuck decode-thread join), and
    // A never publishes (a wedged producer).
    let gate = Arc::new(AtomicBool::new(true));
    let a_shut = Arc::new(AtomicBool::new(false));
    let key_a = SourceKey::from_canonical("rtsp://wedged");
    let ha = reg
        .acquire(
            key_a.clone(),
            size(1920, 1080),
            init("wedged", a_shut.clone(), Some(gate.clone())),
        )
        .unwrap();

    // Entry B: healthy; publishes a frame its consumer can sample.
    let key_b = SourceKey::from_canonical("rtsp://healthy");
    let hb = reg
        .acquire(
            key_b.clone(),
            size(1920, 1080),
            init("healthy", Arc::new(AtomicBool::new(false)), None),
        )
        .unwrap();
    hb.store().publish(99_u64, MediaTime::from_nanos(0));
    assert_eq!(
        hb.store()
            .read_at(MediaTime::from_nanos(0))
            .frame()
            .map(|f| **f),
        Some(99)
    );

    // Drop A's LAST handle → last-release teardown. A's `shutdown` is wedged on the
    // gate forever (until we clear it): a blocking join here would HANG the test.
    let t0 = Instant::now();
    drop(ha);
    let drop_elapsed = t0.elapsed();
    assert!(
        drop_elapsed < Duration::from_millis(250),
        "last-release Drop must be a NON-BLOCKING reaper hand-off, not a join (took {drop_elapsed:?})"
    );
    assert!(!reg.contains(&key_a), "A's entry is removed synchronously");
    assert!(
        !a_shut.load(Ordering::Acquire),
        "A's shutdown must still be wedged (running OFF the hot path, on the reaper)"
    );

    // While A's teardown is wedged, the sibling B keeps sampling with no stall.
    let sample_start = Instant::now();
    for _ in 0..2000 {
        assert_eq!(
            hb.store()
                .read_at(MediaTime::from_nanos(0))
                .frame()
                .map(|f| **f),
            Some(99),
            "sibling B's lock-free sample must never stall while A is wedged (inv #10)"
        );
    }
    assert!(
        sample_start.elapsed() < Duration::from_secs(2),
        "sibling sampling must not be stalled by a wedged sibling"
    );

    // An unrelated acquire/release stays fast — the registry lock is never held
    // across the wedged join.
    let t1 = Instant::now();
    let hc = reg
        .acquire(
            SourceKey::from_canonical("rtsp://c"),
            size(1, 1),
            init("c", Arc::new(AtomicBool::new(false)), None),
        )
        .unwrap();
    drop(hc);
    assert!(
        t1.elapsed() < Duration::from_millis(250),
        "an unrelated acquire/release must not be blocked by a wedged sibling teardown"
    );

    // Clear the wedge; A's shutdown can now complete on the reaper. `reg.shutdown`
    // drains + joins the reaper, so A's shutdown has provably run afterwards.
    gate.store(false, Ordering::Release);
    drop(hb);
    reg.shutdown();
    assert!(
        a_shut.load(Ordering::Acquire),
        "A's shutdown must complete on the reaper once the wedge clears"
    );
}

#[test]
fn acquire_store_shares_one_store_and_grows_supremum() {
    // SLICE 2 adoption (pipeline.rs): the registry owns the shared `TileStore` +
    // its sizing while the decode teardown is still owned externally (the run's
    // `StopRegistry`, until the decode-ownership hoist). `acquire_store` is the
    // store-only sibling of `acquire`: it registers an entry with NO actor (the
    // factory yields only the store). It MUST still decode-once — share ONE store
    // across every reference of one canonical key and grow the per-axis supremum,
    // exactly like `acquire` — and last-release MUST tear the entry down (there is
    // no actor to reap). The `store` accessor returns that same shared `Arc`.
    let reg = SourceRegistry::<u64>::new();
    let key = SourceKey::from_canonical("rtsp://cam-share");
    let calls = Arc::new(AtomicUsize::new(0));

    let counting = |calls: Arc<AtomicUsize>| {
        move |_req: RequestedSize| {
            calls.fetch_add(1, Ordering::Release);
            Ok::<_, Infallible>(store("cam-share"))
        }
    };

    let h1 = reg
        .acquire_store(key.clone(), size(640, 360), counting(calls.clone()))
        .unwrap();
    // A second reference to the SAME key must NOT run the factory and must share
    // the one store (decode-once, use-many).
    let h2 = reg
        .acquire_store(
            key.clone(),
            size(1920, 720),
            |_r| -> Result<Arc<TileStore<u64>>, Infallible> {
                panic!("second reference to an existing key must NOT run the factory");
            },
        )
        .unwrap();

    assert_eq!(
        calls.load(Ordering::Acquire),
        1,
        "one canonical key must create exactly ONE store (decode-once)"
    );
    assert_eq!(reg.active_len(), 1);
    assert!(
        Arc::ptr_eq(h1.store(), h2.store()),
        "both references must share ONE store"
    );
    assert_eq!(
        reg.requested_supremum(&key),
        Some(size(1920, 720)),
        "supremum must grow to the per-axis max across references"
    );
    // The `store` accessor (lifecycle/test) returns the SAME shared Arc — never a
    // clone of the pixels, and it is not the sample path.
    assert!(
        Arc::ptr_eq(&reg.store(&key).unwrap(), h1.store()),
        "the store accessor returns the shared store Arc"
    );

    // Last-release tears the entry down — no actor to reap, just removed.
    drop((h1, h2));
    assert_eq!(
        reg.active_len(),
        0,
        "last release removes the store-only entry"
    );
    assert!(!reg.contains(&key));
    reg.shutdown();
}
