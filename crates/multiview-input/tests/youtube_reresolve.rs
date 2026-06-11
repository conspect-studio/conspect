//! Deterministic tests for the `YouTube` **re-resolution policy + loop** (ADR-0015
//! phases P2–P4): when to refresh the time-limited `*.googlevideo.com` HLS URL,
//! how a resolved manifest becomes an HLS ingest plan, and that a resolve failure
//! degrades the tile (never panics, never back-pressures the engine).
//!
//! These run only with `--features youtube`. They drive the *pure* policy
//! ([`ReresolveSchedule`]) and the loop driver ([`run_reresolve_loop`]) with an
//! **injected fake resolver and a controllable clock** — no network, no
//! subprocess, no real sleeping (tokio time is paused). The make-before-break
//! swap ordering and the 403-burst trigger are pinned here, off the data plane.
#![cfg(feature = "youtube")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_input::youtube::reresolve::{
    run_reresolve_loop, ManualClock, ReresolveConfig, ReresolveOutcome, ReresolveSchedule, Resolver,
};
use multiview_input::youtube::{LiveStatus, ResolvedHls, YoutubeError};

/// Build a `ResolvedHls` whose URL carries the given `expire` Unix timestamp in
/// the standard googlevideo query form, so the policy parses it the same way the
/// real resolver would.
fn resolved_with_expire(tag: &str, expire_unix: i64) -> ResolvedHls {
    let manifest_url = format!(
        "https://manifest.googlevideo.com/api/manifest/hls_playlist/{tag}/index.m3u8?expire={expire_unix}"
    );
    ResolvedHls::new(manifest_url, LiveStatus::Live, Some(expire_unix))
}

// ---------------------------------------------------------------------------
// Pure policy: deadline scheduling + 403-burst trigger.
// ---------------------------------------------------------------------------

#[test]
fn schedule_refreshes_lead_seconds_before_expiry() {
    // 15-minute lead inside a 6-hour upper-bound guard.
    let cfg = ReresolveConfig {
        lead: Duration::from_secs(900),
        ttl_guard: Duration::from_secs(6 * 3600),
        error_burst_threshold: 5,
    };
    // Resolved at t=1000 with an expiry far enough out that the lead, not the
    // upper-bound guard, sets the deadline.
    let resolved = resolved_with_expire("a", 1000 + 3600);
    let sched = ReresolveSchedule::new(cfg, &resolved, 1000);

    // Deadline = expire(4600) - lead(900) = 3700.
    assert_eq!(sched.deadline_unix(), 3700);
    // Not due 1 s before the deadline; due exactly at it and after.
    assert!(!sched.due(3699));
    assert!(sched.due(3700));
    assert!(sched.due(3701));
}

#[test]
fn schedule_falls_back_to_ttl_guard_when_expire_absent() {
    let cfg = ReresolveConfig {
        lead: Duration::from_secs(900),
        ttl_guard: Duration::from_secs(6 * 3600),
        error_burst_threshold: 5,
    };
    // A resolved URL with NO parsed expiry: the upper-bound TTL guard applies.
    let resolved = ResolvedHls::new(
        "https://manifest.googlevideo.com/api/manifest/hls_playlist/x/index.m3u8",
        LiveStatus::Live,
        None,
    );
    let sched = ReresolveSchedule::new(cfg, &resolved, 1000);

    // Deadline = resolved_at(1000) + ttl_guard(21600) - lead(900) = 21700.
    assert_eq!(sched.deadline_unix(), 1000 + 6 * 3600 - 900);
    assert!(!sched.due(21699));
    assert!(sched.due(21700));
}

#[test]
fn schedule_clamps_to_guard_when_expire_is_far_beyond_it() {
    // A stale/implausible expiry far past the 6 h guard must NOT push the
    // deadline out — the upper-bound guard wins (never trust a wild `expire`).
    let cfg = ReresolveConfig {
        lead: Duration::from_secs(900),
        ttl_guard: Duration::from_secs(6 * 3600),
        error_burst_threshold: 5,
    };
    let resolved = resolved_with_expire("a", 1000 + 100 * 3600);
    let sched = ReresolveSchedule::new(cfg, &resolved, 1000);

    // The guard (resolved_at + ttl_guard - lead) caps the deadline below the
    // far-future expiry-derived one.
    assert_eq!(sched.deadline_unix(), 1000 + 6 * 3600 - 900);
}

#[test]
fn segment_errors_trigger_immediate_reresolution_at_threshold() {
    let cfg = ReresolveConfig {
        lead: Duration::from_secs(900),
        ttl_guard: Duration::from_secs(6 * 3600),
        error_burst_threshold: 3,
    };
    let resolved = resolved_with_expire("a", 1_000_000);
    let mut sched = ReresolveSchedule::new(cfg, &resolved, 1000);

    // Below threshold: not yet a burst.
    assert!(!sched.record_segment_error());
    assert!(!sched.record_segment_error());
    // The threshold-th consecutive error trips the immediate-reresolve trigger.
    assert!(sched.record_segment_error());
    // A success clears the burst; the count must start over afterwards.
    sched.reset_errors();
    assert!(!sched.record_segment_error());
}

// ---------------------------------------------------------------------------
// Wiring seam: a resolved manifest URL becomes an HLS ingest target.
// ---------------------------------------------------------------------------

#[test]
fn resolved_manifest_becomes_hls_ingest_url() {
    let resolved = resolved_with_expire("b", 2_000_000);
    // The wiring seam hands libav exactly the resolved `*.googlevideo.com`
    // manifest URL — verbatim, never a reconstructed streamingData URL.
    assert_eq!(
        multiview_input::youtube::reresolve::ingest_url(&resolved),
        resolved.manifest_url.as_str()
    );
}

// ---------------------------------------------------------------------------
// Loop driver: make-before-break swaps, failure degrades (no panic), bounded.
// ---------------------------------------------------------------------------

/// A fake resolver returning a scripted sequence of results (Ok URLs or typed
/// errors), so the loop is driven with no network. Records its call count.
struct FakeResolver {
    results: Mutex<std::collections::VecDeque<Result<ResolvedHls, YoutubeError>>>,
    calls: Arc<std::sync::atomic::AtomicU32>,
}

impl FakeResolver {
    fn new(results: Vec<Result<ResolvedHls, YoutubeError>>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
            calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }
}

impl Resolver for FakeResolver {
    async fn resolve(&self, _url: &str) -> Result<ResolvedHls, YoutubeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.results
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Err(YoutubeError::Unavailable("exhausted".to_owned())))
    }
}

#[tokio::test(start_paused = true)]
async fn loop_swaps_make_before_break_across_an_expiry() {
    // Pins make-before-break ORDERING + exactly-two-publishes: the loop publishes
    // the first URL, then the SECOND, each only once the new URL is in hand (a swap
    // event is emitted only after the fresh resolve succeeds — never a gap). With
    // `expire = 0` (below the injected clock of 1000) the refresh deadline is
    // already in the past, so the second resolve fires WITHOUT a timed wait — this
    // test exercises the swap ordering, not the lead-time delay (the lead-time
    // deadline math is covered by the pure `ReresolveSchedule` tests above).
    let first = resolved_with_expire("v1", 0); // expire below the clock → deadline already passed
    let second = resolved_with_expire("v2", 0);
    let resolver = FakeResolver::new(vec![Ok(first.clone()), Ok(second.clone())]);

    let swaps: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let swaps_sink = Arc::clone(&swaps);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_loop = Arc::clone(&stop);

    let clock = ManualClock::new(1000);
    let cfg = ReresolveConfig {
        lead: Duration::from_secs(1),
        ttl_guard: Duration::from_secs(10),
        error_burst_threshold: 100,
    };

    let handle = tokio::spawn(async move {
        run_reresolve_loop(
            "https://youtube.com/watch?v=v",
            cfg,
            &resolver,
            &clock,
            &stop_loop,
            move |url: String| {
                swaps_sink.lock().expect("lock").push(url);
            },
        )
        .await
    });

    // Let the loop run both resolves (the deadline is already past, so the second
    // fires immediately after the first), then stop. The sleep only yields enough
    // virtual time for both swaps to land before the stop flag is observed.
    tokio::time::sleep(Duration::from_secs(15)).await;
    stop.store(true, Ordering::SeqCst);
    // Nudge time so the interruptible sleep wakes and observes the stop flag.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let outcome = handle.await.expect("loop task joins").expect("loop ok");

    let swaps = swaps.lock().expect("lock").clone();
    // Initial publish + one make-before-break refresh: the URLs swap in order.
    assert_eq!(swaps, vec![first.manifest_url, second.manifest_url]);
    assert_eq!(outcome, ReresolveOutcome::Stopped);
}

#[tokio::test(start_paused = true)]
async fn loop_unresolvable_url_degrades_without_panic() {
    // The very first resolve fails: the loop must NOT panic, must publish NO URL
    // (the tile rides NO_SIGNAL), and must surface the typed error class so the
    // supervisor can alarm + back off — never crash the engine (inv #1/#10).
    let resolver = FakeResolver::new(vec![Err(YoutubeError::Resolve(
        "extractor broke (n-sig rotated)".to_owned(),
    ))]);
    let swaps: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let swaps_sink = Arc::clone(&swaps);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_loop = Arc::clone(&stop);
    let clock = ManualClock::new(1000);
    let cfg = ReresolveConfig {
        lead: Duration::from_secs(1),
        ttl_guard: Duration::from_secs(10),
        error_burst_threshold: 100,
    };

    let handle = tokio::spawn(async move {
        run_reresolve_loop(
            "https://youtube.com/watch?v=dead",
            cfg,
            &resolver,
            &clock,
            &stop_loop,
            move |url: String| {
                swaps_sink.lock().expect("lock").push(url);
            },
        )
        .await
    });

    // Give the loop time to attempt + back off, then stop it.
    tokio::time::sleep(Duration::from_secs(5)).await;
    stop.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(60)).await;
    let outcome = handle.await.expect("loop task joins").expect("loop ok");

    // No URL was ever published — the tile degrades; the loop stopped cleanly.
    assert!(swaps.lock().expect("lock").is_empty());
    assert_eq!(outcome, ReresolveOutcome::Stopped);
}

// ---------------------------------------------------------------------------
// Live end-to-end (network-gated): resolve a REAL YouTube live URL with the
// real `yt-dlp` resolver and assert it yields a future-dated HLS master.
// ---------------------------------------------------------------------------

/// End-to-end resolution against a real live `YouTube` URL. This needs the
/// network, a current `yt-dlp` on `PATH`, and a *currently live* stream, none of
/// which exist in CI — so it is `#[ignore]`d and only runs when an operator points
/// it at a live URL via `MULTIVIEW_YOUTUBE_LIVE_URL` and runs with `--ignored`.
///
/// It proves the production resolver path end to end: the resolved URL is a
/// `*.googlevideo.com` HLS master with a parsed `expire` in the future — the exact
/// input the HLS ingest path opens and the re-resolution loop schedules against.
#[tokio::test]
#[ignore = "needs network + a live YouTube stream + yt-dlp (set MULTIVIEW_YOUTUBE_LIVE_URL)"]
async fn live_youtube_resolves_to_a_future_dated_hls_master() {
    use multiview_input::youtube::reresolve::{
        ProcessResolver, Resolver, SystemUnixClock, UnixClock,
    };

    let Ok(url) = std::env::var("MULTIVIEW_YOUTUBE_LIVE_URL") else {
        panic!("set MULTIVIEW_YOUTUBE_LIVE_URL to a currently-live YouTube watch URL");
    };

    let process = ProcessResolver::default();
    let master = process
        .resolve(&url)
        .await
        .expect("a live youtube url must resolve to an HLS master");

    assert_eq!(master.live_status, LiveStatus::Live);
    assert!(
        master.manifest_url.contains("googlevideo.com"),
        "resolved master must be a googlevideo HLS URL: {}",
        master.manifest_url
    );
    // The resolved URL must be valid into the future — otherwise the
    // re-resolution loop would have nothing to schedule against.
    let now = SystemUnixClock.now_unix();
    let expire = master
        .expire_unix
        .expect("a resolved live HLS master carries an expire deadline");
    assert!(
        expire > now,
        "resolved master already expired (expire={expire}, now={now})"
    );
}
