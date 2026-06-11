//! `YouTube` live ingest via an external, runtime-discovered **`yt-dlp`** resolver
//! (ADR-0015), behind the off-by-default `youtube` feature.
//!
//! `YouTube` publishes no stable manifest URL: the playable HLS master lives behind
//! the private `InnerTube` `player` API and a frequently-rotated JavaScript
//! `n`-signature. Rather than own that fast-moving scraping surface on a
//! bulletproof-output engine, Multiview delegates extraction to `yt-dlp` — run as
//! an out-of-process subprocess, **discovered at runtime, never vendored or
//! linked** — and consumes only its processed output URL. The resolved
//! `*.googlevideo.com` HLS master is then fed into the standard HLS ingest path; a
//! `YouTube` source is a thin wrapper over `hls`.
//!
//! ## Split: pure core vs subprocess shell
//!
//! * [`resolve`] — the **pure** core (this is the correctness load, ADR-0015
//!   phase P0): [`parse_info_dict`] turns a `yt-dlp -J` JSON info-dict into a
//!   [`ResolvedHls`], classifies [`LiveStatus`], and [`parse_expire`] reads the
//!   `expire` deadline off a resolved URL. No network, no subprocess — fully
//!   fixture- and property-tested.
//! * [`process`] — the thin subprocess shell: discover and spawn `yt-dlp` with an
//!   argument vector (no shell), a hard timeout (a hung process is killed, not
//!   awaited — invariant #10), captured stderr, and the `web_safari` player
//!   client pinned.
//!
//! ## Isolation (invariants #1 / #2 / #10)
//!
//! Resolution and the (later) re-resolution loop run **off the data plane**. A
//! resolution failure degrades the *tile* (LIVE → STALE → `NO_SIGNAL`) and raises
//! an operator alarm; it never stalls the output clock. Re-resolution before the
//! `expire` deadline and the make-before-break URL swap are a later item; this
//! module supplies the resolver core they build on.
//!
//! ## Licensing and terms of service
//!
//! `yt-dlp` is public-domain (Unlicense) and run as a subprocess, so no linking
//! or copyleft obligation attaches; the default Multiview build excludes the
//! feature entirely (LGPL-clean by construction). Cookies, when used, are
//! secret-ref only (ADR-M006). Lawful use is the operator's responsibility
//! (ADR-0015 §7).

pub mod process;
pub mod reresolve;
pub mod resolve;

pub use process::{probe_version, resolve as resolve_url, ResolverConfig};
pub use reresolve::{
    ingest_url, run_reresolve_loop, ManualClock, ProcessResolver, ReresolveConfig,
    ReresolveOutcome, ReresolveSchedule, Resolver, SystemUnixClock, UnixClock,
};
pub use resolve::{parse_expire, parse_info_dict, LiveStatus, ResolvedHls};

/// Errors raised by the `YouTube` resolver.
///
/// The pure-parse variants ([`Json`](YoutubeError::Json),
/// [`NotLive`](YoutubeError::NotLive), [`NoHlsMaster`](YoutubeError::NoHlsMaster))
/// come from the I/O-free core; the subprocess variants
/// ([`Unavailable`](YoutubeError::Unavailable),
/// [`Resolve`](YoutubeError::Resolve)) come from the spawn shell. Marked
/// `#[non_exhaustive]`: downstream `match` statements include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum YoutubeError {
    /// The `yt-dlp -J` output was not a well-formed info-dict.
    #[error("yt-dlp -J output is not valid json: {0}")]
    Json(#[from] serde_json::Error),

    /// The stream is not currently live, so it has no playable live HLS master.
    /// Carries the classified status (upcoming / post-live / VOD).
    #[error("youtube source is not live (status: {0:?})")]
    NotLive(LiveStatus),

    /// A live stream carried no `m3u8_native` format with a `manifest_url` — no
    /// HLS master to feed libav.
    #[error("youtube live source exposes no hls master manifest")]
    NoHlsMaster,

    /// The `yt-dlp` binary could not be spawned, was not found, or timed out. The
    /// capability is reported unavailable rather than crashing the engine.
    #[error("yt-dlp is unavailable: {0}")]
    Unavailable(String),

    /// `yt-dlp` ran but exited unsuccessfully (an extraction failure — e.g. a
    /// rotated player JS / n-sig, or an anti-bot block). Carries its (bounded,
    /// captured) stderr for diagnosis.
    #[error("yt-dlp extraction failed: {0}")]
    Resolve(String),
}
