//! Cast streams (ADR-0111, design §3): the registry of scoped, short-lived
//! cast stream tokens that let a Cast device (which has no login) fetch one
//! cast output's HLS files at `/cast/<token>/<file>`.
//!
//! * A token is 256 bits from the OS CSPRNG, base64url, kept only as its
//!   SHA-256 digest ([`token`]); a lookup compares every digest in constant
//!   time, so neither the registry nor its `Debug` holds a usable token.
//! * Lifetime: `Pending` (created, no playlist fetched yet) expires
//!   [`PENDING_TTL`] after creation; `Live` expires [`IDLE_TTL`] after the
//!   last successful playlist fetch; nothing outlives [`MAX_LIFETIME`].
//!   `DELETE` revokes at once.
//! * Bounds: [`MAX_STREAMS`] tokens, [`MAX_STREAMS_PER_PRINCIPAL`] per key
//!   id; per-stream diagnostics are fixed-size (user agent cut to
//!   [`MAX_USER_AGENT`] characters, the last receiver report at most
//!   [`MAX_REPORT_BYTES`]); the creation limiter tracks at most
//!   [`MAX_LIMITED_PRINCIPALS`] principals.
//! * Output lifecycle: the first stream on a cast output activates it (the
//!   route does that); when the last one is gone the output lingers
//!   [`LINGER`] and [`CastStreams::sweep`] then names it so the caller
//!   deactivates it (and takes it back to Program when the cast API moved
//!   its feed).
//!
//! Isolation (invariant #10): one short `Mutex`, never held across an
//! `.await`; nothing here touches an engine channel.

pub mod lifecycle;
pub(crate) mod reauth;
mod registry;
pub mod token;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use registry::Registry;
pub use registry::{Claim, ClaimError, Ended, ReportError, StreamView};

/// A never-fetched token expires this long after creation.
pub const PENDING_TTL: Duration = Duration::from_secs(90);
/// A live token expires this long after its last successful playlist fetch.
pub const IDLE_TTL: Duration = Duration::from_secs(60);
/// No token outlives this.
pub const MAX_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
/// Live tokens across all principals.
pub const MAX_STREAMS: usize = 32;
/// Live tokens per principal (API key id).
pub const MAX_STREAMS_PER_PRINCIPAL: usize = 8;
/// An output whose last stream ended stays active this long.
pub const LINGER: Duration = Duration::from_secs(10);
/// Receiver user-agent characters kept.
pub const MAX_USER_AGENT: usize = 160;
/// Largest receiver report kept (bytes of JSON).
pub const MAX_REPORT_BYTES: usize = 2048;
/// One receiver report per stream per this interval.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Stream creations per principal: burst.
pub const CREATE_BURST: u32 = 4;
/// Stream creations per principal: one more every this long.
pub const CREATE_REFILL: Duration = Duration::from_secs(1);
/// Principals the creation limiter tracks (oldest evicted first).
pub const MAX_LIMITED_PRINCIPALS: usize = 1024;
/// Server-side revocations remembered for their owner's `GET` (oldest
/// forgotten first; each also ages out after [`MAX_LIFETIME`]).
pub const MAX_REVOKED: usize = 2 * MAX_STREAMS;
/// The background sweep period (a lazy sweep also runs on every API call).
pub const SWEEP_PERIOD: Duration = Duration::from_secs(5);

/// A wall clock in Unix milliseconds (injectable for tests).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
}

/// What fetched the media of a stream (no token, no IP).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CastFetches {
    /// Successful playlist fetches.
    pub playlists: u64,
    /// Successful segment / init fetches.
    pub segments: u64,
    /// The last fetch (Unix ms), if any.
    pub last_unix_ms: Option<u64>,
    /// The last fetcher's `User-Agent` (at most 160 characters).
    pub user_agent: Option<String>,
    /// The last fetcher was a Cast device (`User-Agent` contains `CrKey`).
    pub cast_device: bool,
}

/// The last fetch of the receiver app (no IP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReceiverFetch {
    /// When (Unix ms).
    pub unix_ms: u64,
    /// The fetcher's `User-Agent` (at most 160 characters).
    pub user_agent: Option<String>,
    /// The fetcher was a Cast device (`User-Agent` contains `CrKey`).
    pub cast_device: bool,
}

/// The last receiver report of a stream, as received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReceiverReport {
    /// When it arrived (Unix ms).
    pub received_unix_ms: u64,
    /// The report object the receiver posted (at most 2 KiB of JSON).
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub report: serde_json::Value,
}

/// A user agent as kept: at most [`MAX_USER_AGENT`] characters.
#[must_use]
pub fn bounded_user_agent(raw: Option<&str>) -> Option<String> {
    raw.map(|ua| ua.chars().take(MAX_USER_AGENT).collect())
}

/// The cast stream registry (installed in [`crate::AppState::cast_streams`]).
pub struct CastStreams {
    inner: Mutex<Registry>,
    clock: Clock,
    sweeper: std::sync::atomic::AtomicBool,
    /// The registered custom receiver app id (8 upper-case hex characters).
    app_id: Mutex<Option<String>>,
}

impl std::fmt::Debug for CastStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("CastStreams")
            .field("streams", &inner.len())
            .finish_non_exhaustive()
    }
}

impl Default for CastStreams {
    fn default() -> Self {
        Self::new()
    }
}

impl CastStreams {
    /// An empty registry on the system clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(system_clock())
    }

    /// An empty registry on `clock` (tests).
    #[must_use]
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Registry::default()),
            clock,
            sweeper: std::sync::atomic::AtomicBool::new(false),
            app_id: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set the registered custom receiver app id (already validated).
    pub fn set_receiver_app_id(&self, app_id: Option<String>) {
        *self.app_id.lock().unwrap_or_else(PoisonError::into_inner) = app_id;
    }

    /// The registered custom receiver app id, if any.
    #[must_use]
    pub fn receiver_app_id(&self) -> Option<String> {
        self.app_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `true` exactly once: the caller starts the sweep task.
    pub(crate) fn claim_sweeper(&self) -> bool {
        !self.sweeper.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Now, in Unix ms, on this registry's clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        (self.clock)()
    }
}

mod api;
pub use api::{OwnedStream, SlotView};

#[cfg(test)]
mod tests;
