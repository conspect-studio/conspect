//! The `YouTube` **re-resolution** policy and supervised loop (ADR-0015 phases
//! P2–P4): the mechanism for keeping a live tile alive across the ~6 h
//! `*.googlevideo.com` HLS URL expiry by refreshing the manifest URL ahead of the
//! `expire` deadline, off the data plane.
//!
//! A resolved googlevideo URL is time-limited — it carries an `expire` Unix
//! timestamp after which the CDN returns HTTP 403 (for live HLS the window is
//! roughly six hours; see [`super::resolve::parse_expire`]). A long-running
//! ingest that does nothing watches its tile go dark at that boundary. This module
//! provides the loop that prevents that. The loop:
//!
//! * parses the deadline and refreshes **`lead`** seconds ahead of it
//!   ([`ReresolveSchedule`]), falling back to a **`ttl_guard`** upper bound when
//!   `expire` is absent and clamping to that guard when `expire` is implausibly
//!   far out (never trust a wild value);
//! * re-resolves **immediately** on a sustained segment-fetch (403) error burst,
//!   in case the TTL estimate was wrong or `YouTube` rotated the URL early;
//! * does a **make-before-break** swap (ADR-R004/ADR-M005 Class-1 style): the new
//!   URL is resolved and validated *before* it is handed to the HLS ingest path,
//!   so the tile holds last-good at worst, never blinks; and
//! * runs as a **fallible supervised subtask** — a resolve failure degrades the
//!   *tile* (LIVE → STALE → `NO_SIGNAL`) and backs off ([`super::super::reconnect`]),
//!   it never panics, never blocks the output clock (invariant #1), and never
//!   back-pressures the engine (invariant #10).
//!
//! ## Pure policy vs the async driver
//!
//! [`ReresolveSchedule`] is the **pure** policy — deadline math + the error-burst
//! trigger — fully deterministic and unit-tested with an injected clock. The async
//! [`run_reresolve_loop`] is the thin driver: it is generic over an injected
//! [`Resolver`] (the real one spawns `yt-dlp`; tests inject canned results) and a
//! [`UnixClock`], and publishes each fresh URL through a caller-supplied swap sink.
//! It does no decoding of its own — the resolved manifest is fed to the standard
//! HLS ingest path, which owns the actual demux/decode.
//!
//! ## Wiring status (honest, no aspiration)
//!
//! [`ReresolveSchedule`] + [`run_reresolve_loop`] are implemented and unit-tested
//! here, but the **proactive lead-time loop is not yet spawned by the CLI** — that
//! bridge (the async loop ↔ the synchronous std decode thread, with a swappable
//! URL slot) is the remaining slice (tracked as **IN-5b**). What the CLI ingest
//! path (`multiview-cli`'s `open_and_stream`) does **today** is re-resolve a fresh
//! master on every (re)connect via the existing reconnect/backoff bracket: a
//! long run still survives the ~6 h expiry — the segment fetch 403s, the reconnect
//! re-resolves, and the tile rides LIVE → STALE → reconnect back to LIVE — but the
//! tile **does briefly degrade at the boundary** (break-before-make) rather than
//! refreshing ahead of it. Wiring the loop above into ingest upgrades that to the
//! make-before-break, refresh-before-expiry behaviour this module already provides.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::reconnect::{Backoff, BackoffConfig, JITTER_SCALE};

use super::resolve::ResolvedHls;
use super::{process::resolve as process_resolve, ResolverConfig, YoutubeError};

/// Default refresh lead time: re-resolve this far **before** the parsed `expire`
/// deadline. 15 minutes inside a ~6 h live-HLS window (ADR-0015 §4 / docs §4).
pub const DEFAULT_LEAD: Duration = Duration::from_secs(900);

/// Default upper-bound TTL guard for a resolved live-HLS URL. Used as the refresh
/// horizon when no `expire` is parsed, and as a clamp so an implausibly distant
/// `expire` can never push the refresh out (the ~6 h figure is approximate — the
/// parsed `expire` is authoritative *within* this guard, ADR-0015 §4).
pub const DEFAULT_TTL_GUARD: Duration = Duration::from_secs(6 * 3600);

/// Default consecutive segment-fetch (403) errors that trip an immediate
/// re-resolution, in case the TTL estimate was wrong or `YouTube` rotated the URL
/// early (ADR-0015 §4 "belt-and-braces").
pub const DEFAULT_ERROR_BURST_THRESHOLD: u32 = 5;

/// How re-resolution timing is tuned.
///
/// A plain configuration record (not `#[non_exhaustive]`) so callers and tests can
/// build it directly; [`ReresolveConfig::default`] yields the ADR-0015 defaults.
#[derive(Debug, Clone, Copy)]
pub struct ReresolveConfig {
    /// Re-resolve this long **before** the parsed `expire` deadline (the refresh
    /// lead time).
    pub lead: Duration,
    /// Upper-bound TTL guard: the refresh horizon when `expire` is absent, and the
    /// clamp applied to a far-future `expire` so it cannot defer the refresh.
    pub ttl_guard: Duration,
    /// Consecutive segment-fetch (403) errors that trip an immediate re-resolve.
    pub error_burst_threshold: u32,
}

impl Default for ReresolveConfig {
    fn default() -> Self {
        Self {
            lead: DEFAULT_LEAD,
            ttl_guard: DEFAULT_TTL_GUARD,
            error_burst_threshold: DEFAULT_ERROR_BURST_THRESHOLD,
        }
    }
}

/// A source of the current wall-clock instant in **Unix seconds** (seconds since
/// 1970-01-01), injected so the re-resolution policy is deterministically testable
/// without real time.
pub trait UnixClock {
    /// The current time in whole Unix seconds.
    fn now_unix(&self) -> i64;
}

/// The production clock: reads the system wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemUnixClock;

impl UnixClock for SystemUnixClock {
    fn now_unix(&self) -> i64 {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            // A pre-epoch clock is implausible; treat it as the epoch so the
            // schedule still produces a finite, sane deadline (never panics).
            Err(_) => 0,
        }
    }
}

/// A fixed clock for deterministic tests and for the loop driver's injection.
/// Returns a constant Unix-seconds value (the loop computes a sleep *duration*
/// from it and then uses real/virtual elapsed time for the wait).
#[derive(Debug, Clone, Copy)]
pub struct ManualClock {
    now_unix: i64,
}

impl ManualClock {
    /// Construct a clock pinned at `now_unix` Unix seconds.
    #[must_use]
    pub const fn new(now_unix: i64) -> Self {
        Self { now_unix }
    }
}

impl UnixClock for ManualClock {
    fn now_unix(&self) -> i64 {
        self.now_unix
    }
}

/// Resolves a `YouTube` watch URL to a live HLS master. Injected into
/// [`run_reresolve_loop`] so the loop is testable with canned results (no
/// network, no subprocess); the production implementation spawns `yt-dlp`.
///
/// `async fn` in traits is used directly (stable). The loop only ever calls
/// `resolve` with a hard-timeout-bounded implementation (the real one is
/// [`super::process::resolve`]); a hung resolver is killed by that timeout, never
/// awaited unbounded (invariant #10).
pub trait Resolver {
    /// Resolve `url` to a live HLS master, or a typed [`YoutubeError`].
    fn resolve(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = Result<ResolvedHls, YoutubeError>> + Send;
}

/// The production resolver: spawns `yt-dlp` with the pinned config (a hard
/// timeout, an argument vector, no shell — see [`super::process`]).
#[derive(Debug, Clone, Default)]
pub struct ProcessResolver {
    /// How `yt-dlp` is located and bounded.
    pub config: ResolverConfig,
}

impl ProcessResolver {
    /// Construct a resolver with an explicit `yt-dlp` invocation config.
    #[must_use]
    pub fn new(config: ResolverConfig) -> Self {
        Self { config }
    }
}

impl Resolver for ProcessResolver {
    async fn resolve(&self, url: &str) -> Result<ResolvedHls, YoutubeError> {
        process_resolve(&self.config, url).await
    }
}

/// The HLS ingest target for a resolved source: the resolved `*.googlevideo.com`
/// manifest URL handed **verbatim** to the standard HLS ingest path (libav), never
/// a reconstructed `streamingData` URL (ADR-0015 §3). This is the wiring seam the
/// CLI uses to turn a re-resolved manifest into a `SourceLocation::Url`-style
/// ingest target without re-implementing decode.
#[must_use]
pub fn ingest_url(resolved: &ResolvedHls) -> &str {
    resolved.manifest_url.as_str()
}

/// The **pure** re-resolution policy: tracks the active resolved URL's refresh
/// deadline and a consecutive segment-error (403) burst counter.
///
/// All timing is in **Unix seconds** and all arithmetic saturates, so the policy
/// never overflows and never panics on a wild `expire` value. The async loop owns
/// the actual waiting; this type only answers *when* and *why* to refresh.
#[derive(Debug, Clone)]
pub struct ReresolveSchedule {
    config: ReresolveConfig,
    /// The Unix-seconds instant at which the active URL should be refreshed
    /// (`lead` before the effective deadline, clamped by the TTL guard).
    deadline_unix: i64,
    /// Consecutive segment-fetch errors since the last [`Self::reset_errors`].
    consecutive_errors: u32,
}

impl ReresolveSchedule {
    /// Build a schedule for a freshly resolved URL.
    ///
    /// The refresh deadline is `lead` before the **effective** expiry, where the
    /// effective expiry is the parsed `expire` when present, clamped to
    /// `resolved_at + ttl_guard` (the upper-bound guard). When `expire` is absent
    /// the guard horizon is used directly. `resolved_at` is the Unix-seconds
    /// instant the URL was resolved (the caller's [`UnixClock`] reading).
    #[must_use]
    pub fn new(config: ReresolveConfig, resolved: &ResolvedHls, resolved_at: i64) -> Self {
        let guard_secs = i64::try_from(config.ttl_guard.as_secs()).unwrap_or(i64::MAX);
        let lead_secs = i64::try_from(config.lead.as_secs()).unwrap_or(i64::MAX);
        let guard_expiry = resolved_at.saturating_add(guard_secs);
        // Effective expiry: trust the parsed `expire`, but never let it exceed the
        // upper-bound guard (a far-future/implausible value must not defer the
        // refresh). With no parsed expire, the guard horizon is authoritative.
        let effective_expiry = match resolved.expire_unix {
            Some(expire) => expire.min(guard_expiry),
            None => guard_expiry,
        };
        let deadline_unix = effective_expiry.saturating_sub(lead_secs);
        Self {
            config,
            deadline_unix,
            consecutive_errors: 0,
        }
    }

    /// The Unix-seconds instant at which the active URL should be refreshed.
    #[must_use]
    pub const fn deadline_unix(&self) -> i64 {
        self.deadline_unix
    }

    /// Whether the refresh deadline has been reached at `now_unix` (a scheduled,
    /// lead-time refresh is due).
    #[must_use]
    pub const fn due(&self, now_unix: i64) -> bool {
        now_unix >= self.deadline_unix
    }

    /// Record one segment-fetch (403/error) from the HLS reader and report whether
    /// the consecutive-error burst has reached the configured threshold — i.e.
    /// whether to re-resolve **immediately** (belt-and-braces, ADR-0015 §4).
    ///
    /// A threshold of `0` is treated as `1` (any single error triggers).
    pub fn record_segment_error(&mut self) -> bool {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        self.consecutive_errors >= self.config.error_burst_threshold.max(1)
    }

    /// Clear the consecutive-error burst after a successful segment fetch or a
    /// fresh resolve, so an old burst never lingers into a healthy window.
    pub fn reset_errors(&mut self) {
        self.consecutive_errors = 0;
    }
}

/// How the re-resolution loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReresolveOutcome {
    /// The stop flag was observed; the loop exited cleanly.
    Stopped,
}

/// The base reconnect backoff for a failing re-resolution (mirrors the live-source
/// reconnect base): the first retry waits in `[0, BASE]`, escalating per attempt.
const RERESOLVE_BACKOFF_BASE: Duration = Duration::from_secs(2);

/// The reconnect backoff ceiling for a failing re-resolution: a resolver that
/// keeps failing (e.g. a rotated player JS) retries at most this often, so it
/// never hot-loops `yt-dlp`.
const RERESOLVE_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How often the interruptible wait re-checks the stop flag while sleeping toward
/// the refresh deadline. Short so a stop/teardown is observed promptly without
/// busy-waiting.
const STOP_POLL: Duration = Duration::from_secs(1);

/// Run the supervised re-resolution loop until `stop` is set (invariant #10: the
/// loop only ever *writes* a fresh URL through `swap`; it never blocks the engine).
///
/// On entry it resolves `watch_url` once (with bounded backoff on failure) and,
/// on success, publishes the resolved HLS URL through `swap` — the make-before-
/// break handoff to the HLS ingest path. It then sleeps (interruptibly) until the
/// [`ReresolveSchedule`] deadline, re-resolves, and on success publishes the **new**
/// URL through `swap` *only after it is in hand* (the old URL stays live until then;
/// the tile holds last-good at worst). A resolve failure logs, backs off via the
/// shared [`Backoff`] policy, and retries — degrading the tile, never panicking,
/// never stalling the output clock (invariant #1). Returns when `stop` is observed.
///
/// `swap` is the caller's seam to the ingest path: it receives each freshly
/// resolved manifest URL (see [`ingest_url`]). It must be cheap and non-blocking
/// (e.g. a channel send / lock-free store write); the loop holds no lock across it.
///
/// # Errors
///
/// Never returns `Err` for an extraction failure — those are *expected and
/// handled* (the tile degrades, the loop backs off). The `Result` is reserved so
/// future fatal-configuration faults can surface without `unwrap`; today only
/// [`ReresolveOutcome::Stopped`] is produced.
pub async fn run_reresolve_loop<R, C, S>(
    watch_url: &str,
    config: ReresolveConfig,
    resolver: &R,
    clock: &C,
    stop: &AtomicBool,
    mut swap: S,
) -> Result<ReresolveOutcome, YoutubeError>
where
    R: Resolver,
    C: UnixClock,
    S: FnMut(String),
{
    let mut backoff = Backoff::new(BackoffConfig {
        base: RERESOLVE_BACKOFF_BASE,
        max: RERESOLVE_BACKOFF_MAX,
        factor: 2,
    });

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(ReresolveOutcome::Stopped);
        }

        match resolver.resolve(watch_url).await {
            Ok(fresh) => {
                backoff.reset();
                // Make-before-break: the new URL is in hand and validated — hand it
                // to the ingest path now (the old one, if any, is only retired by
                // this swap). The tile never blinks; at worst it held last-good.
                swap(ingest_url(&fresh).to_owned());

                let schedule = ReresolveSchedule::new(config, &fresh, clock.now_unix());
                // Sleep (interruptibly) until the refresh deadline. The schedule's
                // deadline is in Unix seconds; the wait uses elapsed time from the
                // injected clock's reading — in production the system clock and the
                // sleep advance together; under a paused test clock the elapsed
                // sleep drives the refresh deterministically.
                let wait_secs = schedule
                    .deadline_unix()
                    .saturating_sub(clock.now_unix())
                    .max(0);
                let wait = Duration::from_secs(u64::try_from(wait_secs).unwrap_or(u64::MAX));
                if sleep_until_or_stop(wait, stop).await {
                    return Ok(ReresolveOutcome::Stopped);
                }
                // Deadline reached: loop back to re-resolve (make-before-break).
            }
            Err(err) => {
                // Extraction failure is expected and handled: the tile degrades
                // (the caller's store rides LIVE → STALE → NO_SIGNAL) while we back
                // off and retry. Never panic, never propagate as fatal.
                let jitter = backoff_jitter();
                let nap = backoff.next_delay(jitter);
                tracing::warn!(
                    %watch_url,
                    error = %err,
                    attempt = backoff.attempts(),
                    ?nap,
                    "youtube re-resolution failed; backing off (tile degrades)"
                );
                if sleep_until_or_stop(nap, stop).await {
                    return Ok(ReresolveOutcome::Stopped);
                }
            }
        }
    }
}

/// A fresh full-jitter fraction for the reconnect backoff, scaled to
/// [`JITTER_SCALE`]. Uses a process-wide nanosecond reading as an entropy source
/// (no RNG dependency; the loop does not need cryptographic jitter — only
/// de-correlated retries across many sources).
fn backoff_jitter() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    nanos % (JITTER_SCALE + 1)
}

/// Sleep for `wait`, re-checking `stop` every [`STOP_POLL`] so teardown is prompt.
/// Returns `true` if `stop` was observed (the caller should exit), `false` if the
/// full `wait` elapsed. Never blocks a thread — it `await`s `tokio::time::sleep`.
async fn sleep_until_or_stop(wait: Duration, stop: &AtomicBool) -> bool {
    let mut remaining = wait;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let slice = remaining.min(STOP_POLL);
        tokio::time::sleep(slice).await;
        remaining = remaining.saturating_sub(slice);
    }
    stop.load(Ordering::Acquire)
}
