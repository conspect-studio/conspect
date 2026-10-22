//! Management-plane rate limiting — the keyed, lock-free rate limiter that backs
//! the SEC-14 control-plane `DoS` floor.
//!
//! This module is the **pure, clock-injected** core: a [`RateLimiter`] that
//! answers "may this key make one more request right now?" with a [`Decision`].
//! The axum middleware that keys it on the peer IP (pre-auth) and the API-key id
//! (post-auth), and the global concurrency cap, live alongside it and are wired in
//! [`crate::router`] — but the accounting here has no socket, no `AppState`, and no
//! wall clock, so it is exhaustively testable offline (the same seam pattern the
//! rest of this crate uses for clock-driven logic).
//!
//! The pre-auth per-IP guard keys on the `ConnectInfo` peer address, which is
//! installed **only** by [`crate::serve`] / [`crate::serve_router`] (via
//! `into_make_service_with_connect_info`). Serving [`crate::router`] through any
//! other make-service leaves that extension absent, silently disabling the per-IP
//! guard (it fails open — see [`per_ip_rate_limit`]); route the control plane
//! through the blessed serve helpers to keep it active.
//!
//! ## Bounded by construction — the `DoS`-resistance property
//!
//! A rate limiter that grows a per-key map is itself a memory-exhaustion vector:
//! an attacker rotating source addresses inflates the map without bound. This
//! limiter instead hashes every key into a **fixed-size table** of cells
//! ([`RateLimiter::with_hasher`] allocates the cells once and never grows), so
//! memory is `O(cells)` regardless of how many distinct keys are ever seen. Two
//! keys that hash to the same cell **share** a limiter — which can only make
//! limiting *stricter* for that pair, never looser, so the floor is preserved.
//! The hasher is seeded per process (a random [`RandomState`] in production), so
//! an attacker cannot predict or force a collision to target a specific victim.
//!
//! ## Lock-free hot path
//!
//! Each cell is a single [`AtomicU64`] holding a **theoretical arrival time**
//! (`tat`) — the virtual-scheduling (GCRA) formulation of a token bucket, its exact
//! dual: a `tat` running ahead of `now` by up to the burst tolerance `tau` is a
//! bucket that many requests below full. Because the whole per-cell state is one
//! word, [`RateLimiter::check`] is **lock-free** — a load plus a single **strong**
//! compare-and-swap, never a mutex. The CAS is [`AtomicU64::compare_exchange`] (not
//! `_weak`), so it never fails *spuriously*: a retry happens **only** on genuine
//! contention — a concurrent request that advanced this same cell first — and each
//! retry consumes one such real advance. The limiter is therefore **lock-free but
//! not wait-free**: every failed CAS witnesses another thread's successful update, so
//! the system as a whole always makes progress, but there is **no per-call progress
//! bound** — an individual call can be lapped indefinitely while peers keep winning.
//! Only the admitting branch loops; once the cell's burst
//! is spent every further request takes the CAS-free reject path, and the per-key
//! rate limit itself bounds the sustained admit rate — so a single-key flood cannot
//! manufacture unbounded admit-phase contention. A flood concentrated on one cell
//! serialises on that one atomic word for a few nanoseconds and can **never park a
//! Tokio worker holding a lock**, which a blocking `std::sync::Mutex` on the async hot
//! path could.
//!
//! ## Timekeeping
//!
//! [`RateLimiter::check`] takes the current time as an explicit monotonic
//! nanosecond count, so tests drive it deterministically; the middleware supplies
//! it from a monotonic clock. Accounting is integer-only virtual time — never float
//! — and every operation saturates, so a frozen, jumped, wrapped, or **regressed**
//! clock can neither drift, panic, nor grant a bonus (a rewound clock is absorbed
//! by `max(tat, now)`, so it never double-refills).

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body::{Body as HttpBody, Frame, SizeHint};
use multiview_config::limits::ManagementLimits;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::ApiKeyStore;
use crate::problem::Problem;

/// Nanoseconds per second — the fixed-point base for the virtual-scheduling clock.
/// The per-request increment (`increment_ns`) is `NANOS_PER_SEC / refill_per_sec`,
/// i.e. the emission interval `T` of one request at the configured rate.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Rate parameters: a burst `capacity` (max requests admitted at once) replenished
/// at `refill_per_sec` requests per second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rate {
    /// The maximum number of requests permitted in an instantaneous burst.
    pub capacity: u32,
    /// The steady-state replenishment rate, in requests per second.
    pub refill_per_sec: u32,
}

/// The verdict for one request against a key's cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The request is within budget; the cell has been advanced.
    Allowed,
    /// The request exceeds the budget. `retry_after` is the time until the cell has
    /// freed enough for one request — surfaced verbatim in the `Retry-After`
    /// response header.
    Limited {
        /// How long the caller should wait before retrying.
        retry_after: Duration,
    },
}

/// A bounded, fixed-size, sharded, **lock-free** rate limiter keyed by an arbitrary
/// hashable key.
///
/// Each cell is one [`AtomicU64`] holding a theoretical arrival time (`tat`) — the
/// virtual-scheduling (GCRA) dual of a token bucket. See the module docs for the
/// bounded-memory / collision / seeding / lock-free rationale.
pub(crate) struct RateLimiter<S = RandomState> {
    /// The fixed table of cells, each a `tat` timestamp in ns. Allocated once; never
    /// grows.
    cells: Box<[AtomicU64]>,
    /// The per-process-seeded hasher that maps a key to a cell.
    hasher: S,
    /// The emission interval `T`: nanoseconds of virtual time one request consumes
    /// (`NANOS_PER_SEC / refill_per_sec`, rounded UP so the effective rate never
    /// exceeds the configured one). At least 1.
    increment_ns: u64,
    /// The burst tolerance `tau = capacity * T`: how far `tat` may run ahead of
    /// `now` while still admitting — the instantaneous burst.
    tau_ns: u64,
    /// The largest `now` the limiter accepts before clamping, so `tat` (bounded by
    /// `now + tau + T`) always has headroom below [`u64::MAX`] and the accounting
    /// can never saturate. ~584 years of ns — unreachable by a real monotonic clock,
    /// this only makes synthetic/adversarial timestamps total.
    now_ceil: u64,
}

impl RateLimiter<RandomState> {
    /// Build a limiter with `cells` cells and the given [`Rate`], seeded with a
    /// per-process-random hasher so cell placement is not attacker-predictable.
    ///
    /// `cells` is clamped to at least 1.
    pub(crate) fn new(cells: usize, rate: Rate) -> Self {
        Self::with_hasher(cells, rate, RandomState::new())
    }
}

impl<S: BuildHasher> RateLimiter<S> {
    /// Build a limiter with an explicit [`BuildHasher`] — the seam tests use to
    /// make cell placement deterministic.
    ///
    /// `cells` is clamped to at least 1 and `refill_per_sec` to at least 1 (a zero
    /// rate would never replenish; config validation rejects it upstream, this clamp
    /// keeps the accounting total).
    pub(crate) fn with_hasher(cells: usize, rate: Rate, hasher: S) -> Self {
        let n = cells.max(1);
        // Emission interval T = ns per request at the configured rate, rounded UP so
        // the effective steady-state rate is never faster than configured (a DoS
        // floor errs strict). refill is clamped to >= 1.
        let increment_ns = NANOS_PER_SEC.div_ceil(u64::from(rate.refill_per_sec.max(1)));
        // Burst tolerance tau = capacity * T (saturating: an absurd capacity clamps
        // rather than overflows; config bounds it upstream).
        let tau_ns = increment_ns.saturating_mul(u64::from(rate.capacity));
        // Keep tat (<= now + tau + one increment) below u64::MAX for any accepted now.
        let now_ceil = u64::MAX.saturating_sub(tau_ns.saturating_add(increment_ns));
        let cells = (0..n)
            // A fresh cell is `tat = 0` (far in the virtual past ⇒ a full burst).
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells,
            hasher,
            increment_ns,
            tau_ns,
            now_ceil,
        }
    }

    /// The number of cells in the fixed table (never changes after construction —
    /// the bounded-memory guarantee). Test-only: the accessor exists to pin that the
    /// table does not grow.
    #[cfg(test)]
    pub(crate) fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// The cell index a key maps to. Stable for the life of the limiter.
    pub(crate) fn cell_of<K: Hash + ?Sized>(&self, key: &K) -> usize {
        let hash = self.hasher.hash_one(key);
        // Work in u64 then narrow: `idx < n <= usize::MAX`, so the narrowing is
        // lossless. No `as` (workspace-banned) and no panic.
        let n = u64::try_from(self.cells.len()).unwrap_or(u64::MAX).max(1);
        usize::try_from(hash % n).unwrap_or(0)
    }

    /// Account for one request from `key` at monotonic time `now_ns`, and report
    /// whether it is [`Decision::Allowed`] or [`Decision::Limited`].
    ///
    /// Virtual-scheduling (GCRA) accounting: the cell's `tat` is the earliest
    /// virtual time at which the next request may arrive; a request is admitted when
    /// advancing `tat` by one increment keeps it within `tau` of `now`. Lock-free —
    /// a load plus a single **strong** CAS in the common case; under genuine
    /// contention the CAS retries (lock-free, **not** wait-free — every failed CAS
    /// witnesses another thread's successful update and is never spurious, but there
    /// is **no per-call progress bound**; the per-key rate limit bounds the sustained
    /// admit *rate*, not the liveness of any individual call). Only the
    /// admitting branch loops: a rejected request returns without a CAS and never
    /// advances `tat`. Every operation saturates and `now` is clamped, so a frozen,
    /// jumped, wrapped, or regressed clock can neither panic nor grant a bonus request.
    pub(crate) fn check<K: Hash + ?Sized>(&self, key: &K, now_ns: u64) -> Decision {
        let idx = self.cell_of(key);
        // `idx < cells.len()` by construction; fail **open** (never limit) on the
        // impossible out-of-range rather than panic — a DoS floor must not become a
        // self-inflicted outage on an internal accounting slip.
        let Some(cell) = self.cells.get(idx) else {
            return Decision::Allowed;
        };
        // Clamp `now` so `tat` keeps headroom below u64::MAX; only synthetic or
        // adversarial timestamps ever reach the ceiling (a real monotonic ns clock
        // starts near 0 and never approaches ~584 years).
        let now = now_ns.min(self.now_ceil);
        let mut tat = cell.load(Ordering::Relaxed);
        loop {
            // The effective arrival time is max(stored tat, now): a cell idle past
            // its tat resets to `now` (a full burst), and — crucially — a REGRESSED
            // clock can never rewind below the recorded tat, so a wobble grants no
            // bonus request (F3).
            let base = tat.max(now);
            // How far the post-admission tat would sit ahead of `now`, computed as
            // (base - now) + T to sidestep any `now + tau` overflow near u64::MAX.
            let excess = base.saturating_sub(now).saturating_add(self.increment_ns);
            if excess <= self.tau_ns {
                // Within burst: advance tat by one increment, committed with a CAS so
                // a concurrent admit on the same cell can never be lost (the exact
                // accounting a lock would give, without the lock).
                let new_tat = base.saturating_add(self.increment_ns);
                match cell.compare_exchange(tat, new_tat, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return Decision::Allowed,
                    Err(observed) => {
                        // A **strong** CAS, so `Err` means a real concurrent advance of
                        // this cell — never a spurious failure: another admitting thread
                        // committed first. Hint the CPU we are in a spin-retry (cheaper
                        // contention, better SMT yield), then re-read and retry against
                        // the observed `tat`. This is lock-free, NOT wait-free: every
                        // failed CAS witnesses a peer's successful update, so the system
                        // as a whole always makes progress, but there is NO per-call
                        // progress bound — an individual call can be lapped indefinitely
                        // while peers keep winning. Once the burst is
                        // spent every request takes the CAS-free reject path below, and
                        // the per-key rate itself bounds the sustained admit rate, so a
                        // single-key flood cannot manufacture unbounded admit-phase
                        // contention. Falling back to *allowing* after a few tries would
                        // be worse — it would let a flood induce contention to bypass the
                        // very limit it defends — so re-driving the admit accounting is
                        // the right call.
                        core::hint::spin_loop();
                        tat = observed;
                    }
                }
            } else {
                // Over burst: refuse WITHOUT advancing tat (a rejected request never
                // consumes virtual time). The wait until one increment frees is
                // `excess - tau` (> 0 here, so saturating_sub cannot underflow).
                let retry_after_ns = excess.saturating_sub(self.tau_ns);
                return Decision::Limited {
                    retry_after: Duration::from_nanos(retry_after_ns),
                };
            }
        }
    }
}

/// The fixed number of rate-limiter cells per keyed limiter. `O(1)` memory that
/// never grows regardless of how many distinct IPs / keys are ever seen — the
/// bounded-memory property that keeps the `DoS` floor from becoming its own
/// memory-exhaustion vector. Sized generously so a hash collision between the
/// handful of real operators / keys is astronomically unlikely.
const RATE_LIMIT_CELLS: usize = 4096;

/// The `Retry-After` hint on a `503` concurrency shed. Unlike a rate limit there
/// is no exact refill time (a permit frees when some in-flight request
/// completes), so a short fixed hint is offered.
const CONCURRENCY_RETRY_AFTER: Duration = Duration::from_secs(1);

// F4: the config-side ceiling MUST equal the runtime `Semaphore::MAX_PERMITS`, so
// `ManagementLimits::validate` rejects exactly the concurrency caps `Semaphore::new`
// would reject (fail-closed at config load) rather than let the runtime silently
// clamp to a different effective cap. This static assertion fails the build if a
// tokio upgrade ever moves the ceiling out from under the config crate.
const _: () = assert!(
    multiview_config::limits::MAX_CONCURRENT_REQUESTS_CEILING == Semaphore::MAX_PERMITS,
    "config MAX_CONCURRENT_REQUESTS_CEILING must track tokio Semaphore::MAX_PERMITS"
);

/// Accept-level connection admission (SEC-14 #126 R2 / [ADR-W031]): the population
/// bound the request-level caps miss.
///
/// The concurrency + rate caps ([`Limiters`]) engage only *after* a request's headers
/// are parsed, so a flood of half-open, slow-header (or slow-TLS-handshake) connections
/// pins sockets and tasks without ever taking a request permit. The serve loop consults
/// this admission gate at **accept**, before any header parse, and drops an over-cap
/// connection immediately. Two bounds compose:
///
/// * a **global** cap across all peers — one owned [`Semaphore`] permit per admitted
///   connection; and
/// * a **per-source-IP** cap so one source cannot monopolise the global budget
///   (keyed on the peer IP, applied pre-auth).
///
/// Both are released the instant the connection's [`ConnectionGuard`] drops. The per-IP
/// map is bounded by the number of **concurrent distinct** peers (never all-IPs-ever):
/// an entry is **evicted** the moment its count reaches zero, so an attacker rotating
/// source addresses cannot grow it without also holding that many concurrent
/// connections (themselves bounded by the global cap).
///
/// [ADR-W031]: ../../../docs/decisions/ADR-W031.md
pub(crate) struct ConnectionAdmission {
    /// The global cap across all peers: one owned permit per admitted connection.
    global: Arc<Semaphore>,
    /// Live connection counts per source IP, capped at `max_per_ip`. Bounded by the
    /// concurrent distinct-IP count — an entry is removed at zero (never a zero-valued
    /// key that would grow the map unbounded).
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    /// The per-source-IP ceiling (at least 1).
    max_per_ip: usize,
}

impl ConnectionAdmission {
    /// Build an admission gate with a `max_connections` global cap and a
    /// `max_connections_per_ip` per-source-IP cap.
    ///
    /// `max_connections` is clamped to `[1, Semaphore::MAX_PERMITS]` as belt-and-braces:
    /// config validation already rejects `0` and `> MAX_CONCURRENT_REQUESTS_CEILING`
    /// (fail-closed at load), so the clamp is unreachable for a validated config; it only
    /// keeps [`Semaphore::new`] total for a test- or embedder-built value. `max_per_ip`
    /// is clamped to at least 1.
    pub(crate) fn new(max_connections: usize, max_connections_per_ip: usize) -> Arc<Self> {
        let permits = max_connections.clamp(1, Semaphore::MAX_PERMITS);
        Arc::new(Self {
            global: Arc::new(Semaphore::new(permits)),
            per_ip: Mutex::new(HashMap::new()),
            max_per_ip: max_connections_per_ip.max(1),
        })
    }

    /// Try to admit one connection from `ip`. On success returns a [`ConnectionGuard`]
    /// that releases both the global permit and the per-IP slot on drop; returns [`None`]
    /// when either cap is at its limit (the caller must then drop the connection at
    /// accept).
    ///
    /// The global permit is acquired **first** (a `try_acquire_owned`, non-blocking); the
    /// per-IP count is then checked **before** it is incremented, so a per-IP rejection
    /// returns without touching the map (no stray entry) and drops the just-acquired
    /// global permit. A poisoned per-IP lock is recovered, never propagated — a poisoned
    /// lock must not wedge the accept loop.
    pub(crate) fn try_admit(self: &Arc<Self>, ip: IpAddr) -> Option<ConnectionGuard> {
        // Global cap first: an owned permit, released when the guard drops.
        let permit = Arc::clone(&self.global).try_acquire_owned().ok()?;
        // Per-IP cap under the map lock: check BEFORE inserting so a rejected attempt
        // leaves no entry behind (bounded by concurrent distinct IPs).
        {
            let mut map = match self.per_ip.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let count = map.get(&ip).copied().unwrap_or(0);
            if count >= self.max_per_ip {
                // Per-IP full: drop the just-acquired global permit (return None) and
                // leave the map untouched. `permit` releases as it goes out of scope.
                return None;
            }
            map.insert(ip, count + 1);
        }
        Some(ConnectionGuard {
            admission: Arc::clone(self),
            ip,
            _permit: permit,
        })
    }

    /// Test-only: the number of distinct source IPs currently tracked (the per-IP map
    /// size). Proves the evict-at-zero bound — a live guard adds exactly one entry and
    /// its drop removes it.
    #[cfg(test)]
    pub(crate) fn tracked_ips(&self) -> usize {
        match self.per_ip.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

/// RAII admission guard for one accepted connection: holds one global [`Semaphore`]
/// permit and one per-IP slot for the connection's whole life. Drop releases the permit
/// and decrements the per-IP count, **evicting** the entry when it reaches zero.
pub(crate) struct ConnectionGuard {
    /// The gate this guard was admitted through — decremented on drop.
    admission: Arc<ConnectionAdmission>,
    /// The source IP whose per-IP count this guard holds.
    ip: IpAddr,
    /// The global permit; released to the semaphore when this field drops.
    _permit: OwnedSemaphorePermit,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut map = match self.admission.per_ip.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Decrement this IP's count and evict the entry at zero so the map stays bounded
        // by concurrent distinct IPs. `saturating_sub` guards the impossible (for a live
        // guard) missing/zero entry rather than underflowing.
        if let Some(count) = map.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
        // The global `OwnedSemaphorePermit` (`_permit`) releases as it drops after this.
    }
}

/// The runtime management-plane limiters (SEC-14), built once from
/// [`ManagementLimits`] and shared behind an `Arc` by the middleware.
///
/// Isolation-safe (invariant #10): purely control-plane, holds no engine handle;
/// the concurrency guard **sheds** (returns `503`) rather than queueing, and the
/// rate guards return `429` — neither ever blocks.
pub(crate) struct Limiters {
    /// The concurrent in-flight request cap ([`None`] ⇒ limits disabled).
    concurrency: Option<Arc<Semaphore>>,
    /// The per-source-IP rate limiter, applied pre-auth.
    per_ip: Option<RateLimiter>,
    /// The per-API-key rate limiter, applied post-auth.
    per_api_key: Option<RateLimiter>,
    /// The monotonic origin the middleware measures `now_ns` against.
    origin: Instant,
    /// Whether any limit is enforced.
    enabled: bool,
}

impl Limiters {
    /// Build the limiters from validated config. A disabled config yields the
    /// inert [`Limiters::disabled`] set.
    pub(crate) fn from_config(cfg: &ManagementLimits) -> Self {
        if !cfg.enabled {
            return Self::disabled();
        }
        // `validate()` rejects `max_concurrent_requests > MAX_PERMITS` at config
        // load (F4), so this `.min` is unreachable after validation; it stays purely
        // as a belt-and-suspenders guard so an unvalidated (e.g. test-constructed)
        // config can never panic `Semaphore::new` — it does NOT silently reshape a
        // real operator's configured cap, which validation has already bounded.
        let permits = cfg.max_concurrent_requests.min(Semaphore::MAX_PERMITS);
        Self {
            concurrency: Some(Arc::new(Semaphore::new(permits))),
            per_ip: Some(RateLimiter::new(
                RATE_LIMIT_CELLS,
                Rate {
                    capacity: cfg.per_ip.burst,
                    refill_per_sec: cfg.per_ip.refill_per_sec,
                },
            )),
            per_api_key: Some(RateLimiter::new(
                RATE_LIMIT_CELLS,
                Rate {
                    capacity: cfg.per_api_key.burst,
                    refill_per_sec: cfg.per_api_key.refill_per_sec,
                },
            )),
            origin: Instant::now(),
            enabled: true,
        }
    }

    /// The inert limiters: the default for the bare [`crate::state::AppState`]
    /// constructor (tests / embedders) and the result of a disabled config. No
    /// middleware is installed when disabled, so this is never on the hot path.
    pub(crate) fn disabled() -> Self {
        Self {
            concurrency: None,
            per_ip: None,
            per_api_key: None,
            origin: Instant::now(),
            enabled: false,
        }
    }

    /// Whether any limit is enforced — drives whether [`crate::router`] installs
    /// the middleware at all.
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The current monotonic time in nanoseconds since construction (saturating).
    fn now_ns(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// The state the per-API-key middleware needs: the shared limiters plus the API
/// key store it resolves the presented `Bearer` credential against, so only a
/// **validated** key is rate-limited (a wrong-secret request cannot drain a
/// victim key's bucket).
#[derive(Clone)]
pub(crate) struct PerKeyLimitState {
    /// The shared runtime limiters.
    pub(crate) limiters: Arc<Limiters>,
    /// The API key store used to resolve the presented credential's `key_id`.
    pub(crate) api_keys: Arc<ApiKeyStore>,
}

/// Round a retry delay up to whole seconds for the `Retry-After` header (RFC 9110
/// §10.2.3 delta-seconds), never below one second.
fn retry_after_secs(retry_after: Duration) -> u64 {
    let round_up = u64::from(retry_after.subsec_nanos() > 0);
    retry_after.as_secs().saturating_add(round_up).max(1)
}

/// Build an RFC-9457 problem response for a limit rejection, with the
/// `Retry-After` header set from `retry_after`.
fn limit_response(
    status: StatusCode,
    slug: &str,
    title: &str,
    detail: &str,
    retry_after: Duration,
) -> Response {
    let mut response = Problem::new(status.as_u16(), slug, title)
        .with_detail(detail.to_owned())
        .into_response();
    // A decimal integer of seconds is always a valid header value; guard anyway.
    if let Ok(value) = HeaderValue::try_from(retry_after_secs(retry_after).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The `429 Too Many Requests` response for a token-bucket rejection.
fn too_many(retry_after: Duration) -> Response {
    limit_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate-limited",
        "Too Many Requests",
        "Request rate limit exceeded; see the Retry-After header.",
        retry_after,
    )
}

/// The `503 Service Unavailable` response for a concurrency shed.
fn overloaded(retry_after: Duration) -> Response {
    limit_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "overloaded",
        "Service Unavailable",
        "The management plane is at its concurrent-request capacity; see the Retry-After header.",
        retry_after,
    )
}

/// Latches the first time [`per_ip_rate_limit`] sees a request with no `ConnectInfo`
/// peer address, so a router served without the connect-info make-service (which
/// silently disables the pre-auth per-IP guard) warns exactly once rather than per
/// request. Process-global; the guard fails open regardless.
static MISSING_CONNECT_INFO_WARNED: std::sync::Once = std::sync::Once::new();

/// Pre-auth per-source-IP rate limit — the brute-force guard on the auth path.
///
/// Keyed on the peer IP from the `ConnectInfo` request extension. That extension is
/// installed **only** by [`crate::serve`] / [`crate::serve_router`] (via
/// `into_make_service_with_connect_info`); serving [`crate::router`] through any other
/// make-service leaves it absent, which silently disables this guard. A request whose
/// peer IP is unavailable is therefore **not** limited here — the concurrency cap +
/// per-key limit still apply (fail open, never a self-inflicted outage) — but a
/// one-time `warn!` fires so the misconfiguration is diagnosable rather than silent.
pub(crate) async fn per_ip_rate_limit(
    State(limiters): State<Arc<Limiters>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(limiter) = limiters.per_ip.as_ref() else {
        return next.run(request).await;
    };
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.ip());
    let Some(ip) = peer_ip else {
        // No `ConnectInfo`: the router was served without the connect-info make-service
        // (i.e. not via `serve`/`serve_router`). Fail open — the concurrency cap +
        // per-key limit still apply, and a DoS floor must never become a self-inflicted
        // outage — but warn ONCE so this bypass of the pre-auth per-IP guard is
        // diagnosable instead of silent.
        MISSING_CONNECT_INFO_WARNED.call_once(|| {
            tracing::warn!(
                "per-IP rate limit inactive: request has no ConnectInfo peer address; serve \
                 the control plane via multiview_control::serve/serve_router so the pre-auth \
                 per-IP guard can key on the source IP"
            );
        });
        return next.run(request).await;
    };
    match limiter.check(&ip, limiters.now_ns()) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after } => too_many(retry_after),
    }
}

/// Post-auth per-API-key rate limit. Resolves the presented `Bearer` credential
/// against the key store and limits only a **validated** key (keyed on its
/// `key_id`), so one credential cannot monopolise the management plane.
/// Unauthenticated requests pass through — the per-IP + concurrency limits cover
/// them.
pub(crate) async fn per_api_key_rate_limit(
    State(state): State<PerKeyLimitState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(limiter) = state.limiters.per_api_key.as_ref() else {
        return next.run(request).await;
    };
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Ok(principal) = state.api_keys.verify_authorization(header_value) else {
        return next.run(request).await;
    };
    match limiter.check(principal.key_id.as_str(), state.limiters.now_ns()) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after } => too_many(retry_after),
    }
}

/// A response body that owns a concurrency [`OwnedSemaphorePermit`] and delegates to
/// an inner body. The permit releases the instant the stream ends — its terminal
/// frame is polled (`None`, or an error), or, failing that, when the body is dropped
/// (a client disconnect before completion) — so a concurrency permit rides the
/// RESPONSE's lifetime, not merely the handler's return. A long-lived streaming body
/// (SSE at `/api/v1/events`) therefore holds its concurrency slot for as long as it
/// streams (F1), so a flood of held-open streams is shed once the cap is reached.
struct PermitBody {
    /// The wrapped response body.
    inner: Body,
    /// The held concurrency slot. [`PermitBody::poll_frame`] `take()`s and drops it on
    /// the terminal `None`/error, so capacity frees exactly when streaming completes —
    /// not whenever a (possibly-retained) body is finally dropped (F-C). `Option` so
    /// the terminal poll can release early; a client disconnect that drops the body
    /// before completion still releases the permit via this field's own `Drop`.
    permit: Option<OwnedSemaphorePermit>,
}

impl HttpBody for PermitBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // `axum::body::Body` is `Unpin`, so this projection is sound without a
        // `pin-project` dependency.
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        // Terminal at `Ready(None)` (end-of-stream) or `Ready(Some(Err))` (a body error
        // ends the stream): release the concurrency slot NOW — the moment the work
        // completes — rather than waiting for this body to be dropped. `take()` is
        // idempotent, so polling a body past its end never double-releases.
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            drop(self.permit.take());
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The claim slot the in-flight [`concurrency_cap`] permit rides in. The middleware
/// puts the acquired permit here and hands a clone to the handler chain via request
/// extensions; a handler that will outlive its response body — the WebSocket upgrade,
/// whose `101` body is empty but whose session is a **detached task** — CLAIMS it via
/// [`ConcurrencyPermitClaim`] and moves it into the session, so the permit is held for
/// the connection's life. If left unclaimed the middleware reclaims it and ties it to
/// the response body ([`PermitBody`]) instead. `Arc<Mutex<Option<_>>>` is an
/// uncontended one-shot hand-off (the lock only ever wraps a `take()`, never held
/// across an `.await`) — unrelated to the lock-free rate-limiter hot path above.
#[derive(Clone)]
struct CarriedPermit(Arc<Mutex<Option<OwnedSemaphorePermit>>>);

/// Extractor that CLAIMS the in-flight concurrency permit for a handler that must hold
/// it beyond the response body — the WebSocket route, whose upgraded socket runs in a
/// detached task ([`axum::extract::ws::WebSocketUpgrade::on_upgrade`] spawns it). The
/// handler moves the claimed permit into the session so the cap counts live sockets.
/// `None` when limits are disabled (no middleware ⇒ no [`CarriedPermit`]) — the handler
/// then runs unbounded by this cap, exactly as before SEC-14.
pub(crate) struct ConcurrencyPermitClaim(pub(crate) Option<OwnedSemaphorePermit>);

impl<St: Send + Sync> FromRequestParts<St> for ConcurrencyPermitClaim {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &St) -> Result<Self, Self::Rejection> {
        // Take the permit out of the shared slot, if a `concurrency_cap` layer put one
        // there. A poisoned lock still yields its inner Option (recover, never panic).
        let permit = parts.extensions.get::<CarriedPermit>().and_then(|carried| {
            let mut slot = match carried.0.lock() {
                Ok(slot) => slot,
                Err(poisoned) => poisoned.into_inner(),
            };
            slot.take()
        });
        Ok(Self(permit))
    }
}

/// Global concurrent-request cap. Acquires one permit per request; when the cap is
/// reached the request is **shed** with `503` + `Retry-After` rather than queued —
/// bounded, never back-pressures (invariant #10).
///
/// The permit is tied to the *work's* lifetime, not merely the handler's return:
/// - a normal or streaming (SSE) response holds it via [`PermitBody`] until the body
///   ends, so a held-open stream keeps its slot;
/// - the WebSocket route CLAIMS it (via [`ConcurrencyPermitClaim`]) and moves it into
///   the detached upgrade task, so a live socket keeps its slot.
///
/// Either way a single acquisition bounds the connection for its whole life, so a
/// flood of long-lived SSE / WebSocket connections is shed once the cap is reached —
/// not just a burst of fast handler executions.
pub(crate) async fn concurrency_cap(
    State(limiters): State<Arc<Limiters>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(semaphore) = limiters.concurrency.as_ref() else {
        return next.run(request).await;
    };
    let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() else {
        return overloaded(CONCURRENCY_RETRY_AFTER);
    };
    // Park the permit in a shared one-shot slot and hand a clone to the handler chain,
    // so a handler that outlives the response body (the WebSocket upgrade) can claim it.
    let slot = Arc::new(Mutex::new(Some(permit)));
    request
        .extensions_mut()
        .insert(CarriedPermit(Arc::clone(&slot)));
    let response = next.run(request).await;
    // Reclaim the permit unless the handler claimed it. Claimed (WebSocket) ⇒ the
    // session task now owns it; return the response untouched. Unclaimed ⇒ tie it to
    // the response body so it releases when the body (a stream) ends.
    let reclaimed = match slot.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    match reclaimed {
        Some(permit) => {
            let (parts, body) = response.into_parts();
            Response::from_parts(
                parts,
                Body::new(PermitBody {
                    inner: body,
                    permit: Some(permit),
                }),
            )
        }
        None => response,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::BuildHasherDefault;
    use std::time::Duration;

    use proptest::prelude::*;

    use super::{Decision, Rate, RateLimiter};

    /// A deterministic hasher so a test controls exactly which key lands in which
    /// cell (production uses a random-seeded `RandomState`).
    type FixedHasher = BuildHasherDefault<DefaultHasher>;

    const SEC_NS: u64 = 1_000_000_000;

    fn limiter(cells: usize, capacity: u32, refill_per_sec: u32) -> RateLimiter<FixedHasher> {
        RateLimiter::with_hasher(
            cells,
            Rate {
                capacity,
                refill_per_sec,
            },
            FixedHasher::default(),
        )
    }

    #[test]
    fn allows_a_full_burst_then_limits_within_the_same_instant() {
        let l = limiter(64, 3, 1);
        let key = "203.0.113.7";
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        // Fourth request in the same instant: the bucket is empty. Retry-after is
        // one second (one token must accrue at 1 token/second).
        assert_eq!(
            l.check(&key, 0),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn refills_at_the_configured_rate() {
        // capacity 1, 2 tokens/sec → one token every 500 ms.
        let l = limiter(64, 1, 2);
        let key = "203.0.113.8";
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        assert_eq!(
            l.check(&key, 0),
            Decision::Limited {
                retry_after: Duration::from_millis(500),
            }
        );
        // 500 ms later exactly one token has accrued → one more request allowed.
        assert_eq!(l.check(&key, SEC_NS / 2), Decision::Allowed);
    }

    #[test]
    fn distinct_non_colliding_keys_keep_independent_allowances() {
        let l = limiter(256, 1, 1);
        let a = "198.51.100.1";
        let b = "198.51.100.2";
        // The test is only meaningful if the two keys occupy different cells.
        assert_ne!(
            l.cell_of(&a),
            l.cell_of(&b),
            "test fixture invalid: keys collide into one cell"
        );
        assert_eq!(l.check(&a, 0), Decision::Allowed);
        // `b` still has its own full bucket even though `a` is now drained.
        assert_eq!(l.check(&b, 0), Decision::Allowed);
        assert_eq!(
            l.check(&a, 0),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn a_single_cell_table_forces_all_keys_to_share_one_bucket() {
        // cells == 1 → every key hashes to the one bucket: bounded memory taken to
        // the extreme. With capacity 1, the SECOND distinct key in the same
        // instant is limited, proving keys share the fixed table (a per-key map
        // would instead hand `key-b` its own fresh bucket → Allowed).
        let l = limiter(1, 1, 1);
        assert_eq!(l.check(&"key-a", 0), Decision::Allowed);
        assert_eq!(
            l.check(&"key-b", 0),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
            }
        );
        assert_eq!(l.cell_count(), 1);
    }

    #[test]
    fn table_never_grows_no_matter_how_many_distinct_keys_are_seen() {
        let l = limiter(128, 5, 5);
        for i in 0..100_000_u32 {
            let _ = l.check(&i, 0);
        }
        // The fixed table is the whole memory footprint — it must not have grown.
        assert_eq!(l.cell_count(), 128);
    }

    proptest! {
        /// At a frozen instant a fresh key admits **exactly** `capacity` requests,
        /// then every further request is limited with a strictly-positive
        /// `retry_after` no larger than the time to refill the whole bucket.
        /// Exercised across arbitrary capacities, rates, keys, and start instants
        /// — this kills off-by-one / mis-accounting mutants in the bucket math.
        #[test]
        fn frozen_burst_admits_exactly_capacity_then_limits(
            capacity in 1_u32..64,
            refill_per_sec in 1_u32..10_000,
            key in any::<u64>(),
            now_ns in any::<u64>(),
            extra in 0_u32..8,
        ) {
            let l = limiter(97, capacity, refill_per_sec);
            // Exactly `capacity` admitted at a single frozen instant.
            for _ in 0..capacity {
                prop_assert_eq!(l.check(&key, now_ns), Decision::Allowed);
            }
            // The next `1 + extra` requests at the same instant are all limited.
            let max_wait_ns = u64::from(capacity)
                .saturating_mul(1_000_000_000)
                .div_ceil(u64::from(refill_per_sec));
            for _ in 0..=extra {
                match l.check(&key, now_ns) {
                    Decision::Allowed => {
                        prop_assert!(false, "an over-capacity request was allowed");
                    }
                    Decision::Limited { retry_after } => {
                        prop_assert!(retry_after > Duration::ZERO);
                        prop_assert!(retry_after <= Duration::from_nanos(max_wait_ns));
                    }
                }
            }
        }
    }

    #[test]
    fn a_clock_wobble_does_not_double_refill() {
        // A non-monotonic injected clock (100 s → 50 s → 100 s) must NOT credit the
        // 50→100 span twice: the limiter accounts against the *latest* arrival time,
        // so only genuine forward progress past the high-water mark replenishes.
        // Capacity 1, refill 1 (one token per second).
        let l = limiter(64, 1, 1);
        let key = "203.0.113.9";
        let t100 = 100 * SEC_NS;
        let t50 = 50 * SEC_NS;

        // Spend the one token at t = 100 s; the next at the same instant is limited.
        assert_eq!(l.check(&key, t100), Decision::Allowed);
        assert!(matches!(l.check(&key, t100), Decision::Limited { .. }));

        // Clock jumps BACK to 50 s: still empty (no negative elapsed refill).
        assert!(matches!(l.check(&key, t50), Decision::Limited { .. }));

        // Clock returns to 100 s: this is NOT new time — the 50→100 span was already
        // accounted the first time we reached 100 s. Re-crediting it here is the bug.
        assert!(
            matches!(l.check(&key, t100), Decision::Limited { .. }),
            "a 100→50→100 clock wobble must not replenish a token twice"
        );

        // Only real forward progress past the high-water mark (101 s) accrues one.
        assert_eq!(l.check(&key, t100 + SEC_NS), Decision::Allowed);
    }
}

#[cfg(test)]
mod middleware_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use multiview_config::limits::ManagementLimits;
    use tower::ServiceExt;

    use super::{
        concurrency_cap, per_api_key_rate_limit, per_ip_rate_limit, Limiters, PerKeyLimitState,
    };
    use crate::auth::{ApiKeyStore, Principal};
    use crate::problem::PROBLEM_JSON;

    /// A limits config with a tight per-IP / per-API-key burst and a slow refill,
    /// so within a fast test the burst is the whole budget (no refill in flight).
    fn cfg(max_concurrent: usize, ip_burst: u32, key_burst: u32) -> ManagementLimits {
        let mut c = ManagementLimits::default();
        c.max_concurrent_requests = max_concurrent;
        c.per_ip.burst = ip_burst;
        c.per_ip.refill_per_sec = 1;
        c.per_api_key.burst = key_burst;
        c.per_api_key.refill_per_sec = 1;
        c
    }

    fn get_ip(uri: &str, ip: &str) -> Request<Body> {
        let mut req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request builds");
        let addr: SocketAddr = format!("{ip}:40000").parse().expect("addr parses");
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn per_ip_limit_allows_the_burst_then_429s_the_same_ip() {
        let limiters = Arc::new(Limiters::from_config(&cfg(256, 1, 1)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, per_ip_rate_limit),
        );

        let first = app
            .clone()
            .oneshot(get_ip("/x", "203.0.113.7"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .clone()
            .oneshot(get_ip("/x", "203.0.113.7"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            second.headers().contains_key(header::RETRY_AFTER),
            "a 429 must carry Retry-After"
        );
        assert_eq!(
            second
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON),
            "a 429 is an RFC-9457 problem+json"
        );
        let body = second.into_body().collect().await.unwrap().to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["status"], 429);
    }

    #[tokio::test]
    async fn per_ip_limit_is_independent_across_ips() {
        let limiters = Arc::new(Limiters::from_config(&cfg(256, 1, 1)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, per_ip_rate_limit),
        );

        // Drain IP A.
        assert_eq!(
            app.clone()
                .oneshot(get_ip("/x", "198.51.100.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(get_ip("/x", "198.51.100.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // IP B is unaffected.
        assert_eq!(
            app.clone()
                .oneshot(get_ip("/x", "198.51.100.2"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn per_ip_limit_fails_open_without_connect_info() {
        // No ConnectInfo extension ⇒ no key ⇒ the per-IP guard cannot limit and
        // passes through (the concurrency cap + per-key limit still apply). Three
        // requests past a burst of 1 all succeed.
        let limiters = Arc::new(Limiters::from_config(&cfg(256, 1, 1)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, per_ip_rate_limit),
        );
        for _ in 0..3 {
            let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    fn per_key_state(cfg: &ManagementLimits) -> (PerKeyLimitState, String) {
        let mut store = ApiKeyStore::new(b"test-pepper".to_vec());
        let principal = Principal::local_admin();
        store.register("op", "s3cret", principal);
        (
            PerKeyLimitState {
                limiters: Arc::new(Limiters::from_config(cfg)),
                api_keys: Arc::new(store),
            },
            "Bearer op.s3cret".to_owned(),
        )
    }

    #[tokio::test]
    async fn per_api_key_limit_429s_a_valid_key_after_its_burst() {
        let (state, bearer) = per_key_state(&cfg(256, 1, 1));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(state, per_api_key_rate_limit),
        );
        let mk = || {
            Request::builder()
                .uri("/x")
                .header(header::AUTHORIZATION, &bearer)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(mk()).await.unwrap().status(),
            StatusCode::OK
        );
        let limited = app.clone().oneshot(mk()).await.unwrap();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn per_api_key_limit_ignores_unauthenticated_requests() {
        let (state, _bearer) = per_key_state(&cfg(256, 1, 1));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(state, per_api_key_rate_limit),
        );
        // No credential ⇒ not per-key limited; three past a burst of 1 all pass.
        for _ in 0..3 {
            let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn concurrency_cap_503s_when_no_permit_is_available() {
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        // Exhaust the single permit up front (in-crate test touches the field).
        let held = limiters
            .concurrency
            .as_ref()
            .expect("enabled ⇒ a semaphore")
            .clone()
            .try_acquire_owned()
            .expect("one permit is free");

        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters.clone(), concurrency_cap),
        );
        let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON)
        );
        drop(held);
    }

    #[tokio::test]
    async fn concurrency_cap_releases_the_permit_after_each_response() {
        // max_concurrent = 1, but sequential requests each release the permit, so
        // both succeed (proves the guard is not a one-shot).
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, concurrency_cap),
        );
        for _ in 0..2 {
            let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    /// A response body that never yields a frame and never ends — a stand-in for an
    /// SSE / long-poll stream that stays open. It lets a test prove the concurrency
    /// permit is tied to the BODY's lifetime (F1), not just the handler's return.
    struct PendingBody;

    impl http_body::Body for PendingBody {
        type Data = axum::body::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Pending
        }

        fn is_end_stream(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn a_held_open_response_body_holds_its_permit_until_the_body_ends() {
        // cap = 1. The handler returns IMMEDIATELY with a body that never completes
        // (an SSE-like long-lived stream). The permit must ride the BODY, not just
        // the handler: while the first response is held open (body undrained) a
        // second request is shed `503`; dropping the first frees the permit.
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        let app = Router::new()
            .route(
                "/stream",
                get(|| async { axum::response::Response::new(Body::new(PendingBody)) }),
            )
            .layer(axum::middleware::from_fn_with_state(
                limiters,
                concurrency_cap,
            ));

        // First request: hold the response object (never drain its still-open body).
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // A second request while the first body is still open: no permit → `503`.
        let second = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a held-open streaming body must keep its concurrency permit"
        );

        // End the first stream (drop the response ⇒ drop its body ⇒ release permit).
        drop(first);
        drop(second);

        // A fresh request now finds the freed permit.
        let third = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_streaming_body_frees_its_permit_at_end_of_stream_not_only_on_drop() {
        // cap = 1. A FINITE streaming body (one data frame, then end-of-stream). The
        // permit must free the instant the body's terminal frame is polled (`None`),
        // not merely when the body object is dropped — otherwise a consumer that drains
        // a response but retains the (exhausted) body pins a concurrency slot (F-C).
        // Proof: drain the first body to completion but KEEP it alive; a second request
        // must still find a free permit.
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        let app = Router::new()
            .route(
                "/body",
                // A whole in-memory body: yields one data frame, then `None`.
                get(|| async { axum::response::Response::new(Body::from("hello")) }),
            )
            .layer(axum::middleware::from_fn_with_state(
                limiters,
                concurrency_cap,
            ));

        // First request: take the response and DRAIN its body to end-of-stream, then
        // hold the exhausted body alive (do NOT drop it).
        let first = app
            .clone()
            .oneshot(Request::builder().uri("/body").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let mut body = first.into_body();
        // Poll every frame through to the terminal `None` (which frees the permit under
        // the fix); the exhausted `body` stays in scope past the loop.
        while let Some(frame) = body.frame().await {
            frame.expect("streamed body frame is Ok");
        }

        // The first body is drained but still held. If the permit only released on
        // drop, it is still occupied here and this second request would be shed 503.
        let second = app
            .clone()
            .oneshot(Request::builder().uri("/body").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::OK,
            "the concurrency permit must free at end-of-stream (the terminal frame poll), \
             not only when the body is dropped"
        );

        drop(body);
    }

    #[tokio::test]
    async fn a_claimed_permit_bounds_a_detached_task_not_just_the_response() {
        // The WebSocket pattern: a handler CLAIMS the in-flight permit (as the real
        // `ws_handler` does, via the same `ConcurrencyPermitClaim` extractor) and moves
        // it into a DETACHED task, returning an immediate empty-body response. The
        // permit must be held by the task — not released when the response returns — so
        // while the task runs a second request is shed `503`. This is the exact fix for
        // the WebSocket half of F1: axum spawns the upgraded socket detached, so tying
        // the permit to the (empty `101`) response body would leave a live socket
        // uncounted.
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        // Keeps the spawned "session" tasks alive across the assertions.
        let keep_alive = Arc::new(tokio::sync::Notify::new());
        let handler_keep = keep_alive.clone();
        let app = Router::new()
            .route(
                "/upgrade",
                get(move |claim: super::ConcurrencyPermitClaim| {
                    let keep = handler_keep.clone();
                    async move {
                        // Move the claimed permit into a detached task (like on_upgrade).
                        tokio::spawn(async move {
                            let _permit = claim.0;
                            keep.notified().await;
                        });
                        axum::response::Response::new(Body::empty())
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                limiters,
                concurrency_cap,
            ));

        // First request: the handler claims the permit into a live detached task.
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/upgrade")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // The detached task still holds the permit → a second request is shed `503`,
        // proving the claim outlives the response (the bug released it at return → 200).
        let second = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/upgrade")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a claimed permit must bound the detached session, not release at response return"
        );

        // Release the detached tasks so they drop their permits (cleanup).
        keep_alive.notify_waiters();
    }

    #[test]
    fn disabled_config_builds_inert_limiters() {
        let mut disabled = ManagementLimits::default();
        disabled.enabled = false;
        let limiters = Limiters::from_config(&disabled);
        assert!(!limiters.is_enabled());
    }
}

#[cfg(test)]
mod connection_admission_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::{IpAddr, Ipv6Addr};

    use super::ConnectionAdmission;

    /// A distinct ULA test address per `n`.
    fn ip(n: u16) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, n))
    }

    #[test]
    fn global_cap_admits_up_to_the_limit_then_rejects() {
        // Global cap 2, per-IP effectively unbounded: the third concurrent connection
        // (even from a fresh IP) is refused at accept once the global cap is reached.
        let admission = ConnectionAdmission::new(2, 100);
        let g1 = admission.try_admit(ip(1)).expect("first admits");
        let g2 = admission.try_admit(ip(2)).expect("second admits");
        assert!(
            admission.try_admit(ip(3)).is_none(),
            "the global cap of 2 must refuse the third concurrent connection"
        );
        // Freeing one slot lets the next connection in.
        drop(g1);
        let _g3 = admission
            .try_admit(ip(3))
            .expect("a freed global slot admits the next connection");
        drop(g2);
    }

    #[test]
    fn per_ip_cap_admits_up_to_the_limit_then_rejects_that_ip() {
        // Global cap generous, per-IP cap 2: a single IP is bounded at 2 while a
        // different IP keeps its own independent allowance.
        let admission = ConnectionAdmission::new(100, 2);
        let _a1 = admission.try_admit(ip(1)).expect("ip1 #1 admits");
        let _a2 = admission.try_admit(ip(1)).expect("ip1 #2 admits");
        assert!(
            admission.try_admit(ip(1)).is_none(),
            "the per-IP cap of 2 must refuse a third connection from the same IP"
        );
        // A different source IP is unaffected by ip1 being at its per-IP cap.
        let _b1 = admission
            .try_admit(ip(2))
            .expect("a different IP keeps its own per-IP allowance");
    }

    #[test]
    fn dropping_a_guard_frees_the_global_slot() {
        let admission = ConnectionAdmission::new(1, 100);
        let g1 = admission.try_admit(ip(1)).expect("first admits");
        assert!(
            admission.try_admit(ip(2)).is_none(),
            "global cap 1 is full while g1 is held"
        );
        drop(g1);
        let _g2 = admission
            .try_admit(ip(2))
            .expect("dropping the guard frees the global slot");
    }

    #[test]
    fn dropping_a_guard_frees_the_per_ip_slot() {
        let admission = ConnectionAdmission::new(100, 1);
        let g1 = admission.try_admit(ip(1)).expect("ip1 #1 admits");
        assert!(
            admission.try_admit(ip(1)).is_none(),
            "per-IP cap 1 is full for ip1 while g1 is held"
        );
        drop(g1);
        let _g2 = admission
            .try_admit(ip(1))
            .expect("dropping the guard frees the per-IP slot for that IP");
    }

    #[test]
    fn a_rejected_per_ip_admission_leaves_no_stray_entry() {
        // A per-IP rejection must not insert or bump a map entry — otherwise a flood
        // of rejected attempts would grow the map. One live guard ⇒ exactly one tracked
        // IP, and the count is not inflated by the rejected attempt (proved by the
        // eviction test: the single live guard's Drop returns the map to empty).
        let admission = ConnectionAdmission::new(100, 1);
        let g1 = admission.try_admit(ip(1)).expect("ip1 #1 admits");
        assert_eq!(
            admission.tracked_ips(),
            1,
            "one live connection ⇒ one tracked IP"
        );
        assert!(
            admission.try_admit(ip(1)).is_none(),
            "ip1 is at its per-IP cap"
        );
        assert_eq!(
            admission.tracked_ips(),
            1,
            "a rejected admission must not add a stray map entry"
        );
        drop(g1);
        assert_eq!(
            admission.tracked_ips(),
            0,
            "the only live guard's Drop must evict the entry (no leftover count from the reject)"
        );
    }

    #[test]
    fn a_per_ip_rejection_releases_its_already_acquired_global_permit() {
        // The reject path is DISTINCT from guard-Drop: `try_admit` takes the global
        // permit FIRST, then may fail the per-IP check and `return None`. That
        // just-acquired permit must be released on the way out (the local permit's RAII
        // drop) — otherwise a single attacker IP already at its per-IP cap would drain
        // one global slot per *rejected* attempt and permanently degrade the global cap
        // for every other source: a slow DoS on the security floor itself. Global cap 4,
        // per-IP cap 1.
        let admission = ConnectionAdmission::new(4, 1);
        // The attacker's one legitimate connection takes its single per-IP slot (and one
        // of the four global slots); hold it so every further attempt from this IP fails
        // the per-IP check AFTER acquiring a global permit.
        let _attacker = admission
            .try_admit(ip(1))
            .expect("the attacker's first connection admits");
        // A flood of over-per-IP attempts. Each acquires a global permit then fails the
        // per-IP check and must release it; a leak would exhaust the three remaining
        // global slots after only three attempts, so ten guarantees exhaustion if the
        // permit is leaked rather than released.
        for _ in 0..10 {
            assert!(
                admission.try_admit(ip(1)).is_none(),
                "an attempt over the per-IP cap must be rejected"
            );
        }
        // The global cap is intact: only the attacker's one legitimate connection holds a
        // slot, so a fresh source IP still admits. This assertion fails iff a rejected
        // attempt leaked its already-acquired global permit.
        let _fresh = admission
            .try_admit(ip(2))
            .expect("a fresh IP still admits — rejected attempts must not leak global slots");
    }

    #[test]
    fn the_per_ip_entry_is_evicted_when_its_last_connection_closes() {
        // The bounded-memory property (nit 1): the per-IP map is bounded by the number
        // of *concurrent distinct* IPs, not all-IPs-ever — so an entry is removed the
        // instant its count reaches zero, not merely decremented.
        let admission = ConnectionAdmission::new(100, 5);
        let g1 = admission.try_admit(ip(1)).expect("ip1 #1 admits");
        let g2 = admission.try_admit(ip(1)).expect("ip1 #2 admits");
        assert_eq!(admission.tracked_ips(), 1, "one distinct IP is tracked");
        drop(g1);
        assert_eq!(
            admission.tracked_ips(),
            1,
            "one connection from ip1 is still live ⇒ the entry remains"
        );
        drop(g2);
        assert_eq!(
            admission.tracked_ips(),
            0,
            "the last connection from ip1 closing must EVICT its map entry (count==0), not leave a \
             zero-valued key that grows the map unbounded"
        );
    }

    #[test]
    fn held_guards_never_exceed_either_cap() {
        // Global cap 4, per-IP cap 2, two source IPs. Exactly 4 admissions succeed
        // (2 per IP), and every further attempt — from either IP — is refused. Proves
        // the two caps compose: neither the global nor a per-IP bound can be exceeded.
        let admission = ConnectionAdmission::new(4, 2);
        let mut held = Vec::new();
        held.push(admission.try_admit(ip(1)).expect("ip1 #1"));
        held.push(admission.try_admit(ip(1)).expect("ip1 #2"));
        assert!(
            admission.try_admit(ip(1)).is_none(),
            "ip1 is at its per-IP cap of 2"
        );
        held.push(admission.try_admit(ip(2)).expect("ip2 #1"));
        held.push(admission.try_admit(ip(2)).expect("ip2 #2"));
        assert_eq!(
            held.len(),
            4,
            "exactly the global cap of 4 connections are live"
        );
        assert!(
            admission.try_admit(ip(2)).is_none(),
            "ip2 is at its per-IP cap of 2 (and the global cap of 4 is full)"
        );
        assert!(
            admission.try_admit(ip(3)).is_none(),
            "a fresh IP is refused because the global cap of 4 is full"
        );
        assert_eq!(
            admission.tracked_ips(),
            2,
            "exactly two distinct IPs are tracked"
        );
    }
}
