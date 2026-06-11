//! The packet-liveness watchdog for guarded passthrough (ADR-0030 §4).
//!
//! A guarded passthrough is **packet-copy while the input is healthy** but
//! **fails to a pre-baked slate** on any disruption. Because nothing is decoded
//! on the copy path, the copy-vs-splice decision cannot use decoded-*picture*
//! age (the framestore [`TileStore`](crate::TileStore) signal). It must instead
//! be driven by **packet liveness** — how recently coded bytes arrived and
//! whether the stream's timestamps are still advancing.
//!
//! [`PacketLiveness`] is therefore a `TileStore` **minus the frame ring**: two
//! wait-free atoms a copy thread Release-stamps, read Acquire-side by the clock
//! thread on every output tick, classified by the same `(elapsed, thresholds)`
//! pure function the tile state machine uses ([`crate::state::classify`]).
//!
//! ## The three signals (ADR-0030 §4 "the watchdog — three signals")
//!
//! 1. **Hard death** — `elapsed_since_last_packet >= splice`, or the caller sets
//!    an explicit Eof/error flag (`read_packet` returned `Eof`/error). ⇒ splice.
//! 2. **Slow-loris / stutter** — packets keep arriving but gaps exceed `hold`
//!    while staying below `splice`: the watchdog rides
//!    [`Live`](PacketLivenessState::Live)/[`Stale`](PacketLivenessState::Stale)
//!    with widened tolerance and **never** splices pre-threshold — no
//!    slate-flapping.
//! 3. **Stalled PTS** — bytes keep flowing (the packet clock is fresh) but the
//!    **max-seen DTS stops advancing** (a frozen encoder looping a stale
//!    timestamp): once `elapsed_since_last_advancing_dts >= pts_stall`, ⇒
//!    splice, even while packets arrive.
//!
//! ## Per-stream, two instances per program
//!
//! A `PacketLiveness` tracks **one** elementary stream. A program holds **two**
//! (video + audio) because per-PID streams fail independently; splice =
//! `video_loss OR audio_loss`, computed by the caller over the two instances.
//!
//! ## Wait-free and fail-safe
//!
//! The only cross-thread coupling is Release/Acquire atomics — no locks, no
//! async, no I/O. Every method that needs "now" takes an injected nanosecond
//! instant from the program's shared monotonic time source; the type never
//! reads a clock itself, so the whole ladder is deterministic and testable. A
//! **stale Acquire read fails safe**: observing an *older* stamp than the true
//! one only ever yields a **larger** elapsed, which biases toward
//! [`Splice`](PacketLivenessState::Splice) — never a false
//! [`Live`](PacketLivenessState::Live).
//!
//! [`TileStore`]: crate::TileStore
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;

use crate::error::{Error, Result};
use crate::state::{classify, TileThresholds};

/// Sentinel for "no packet has ever been recorded" in an arrival-instant atom.
///
/// `i64::MIN` can never be a real arrival instant (the shared time source is
/// monotonic and bounded well away from the `i64` extremes), so it
/// unambiguously encodes the not-yet-stamped state. A reader that observes it
/// treats the stream as dead (it has produced nothing), which is the safe
/// default before the first packet.
const NEVER_RECORDED: i64 = i64::MIN;

/// Sentinel for "no DTS has ever been seen" in the max-seen-DTS atom.
///
/// `i64::MIN` is the additive identity for a running maximum, so the first
/// recorded DTS always advances past it and stamps the advancing-DTS clock.
const NO_DTS_SEEN: i64 = i64::MIN;

/// The watchdog's copy-vs-splice classification for one elementary stream.
///
/// Derived from the same failure ladder as the tile state machine
/// ([`crate::state::classify`]) plus the stalled-PTS and Eof/error overrides.
/// `#[non_exhaustive]` so new rungs can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PacketLivenessState {
    /// Packets arriving freshly and DTS advancing — copy the source bytes.
    Live,
    /// A gap longer than `hold` but shorter than `splice`: ride with widened
    /// tolerance (the slow-loris / stutter band). Still **copy** — never splice
    /// here, to avoid slate-flapping.
    Stale,
    /// Past `splice` (hard death by elapsed or Eof/error) **or** the max-seen
    /// DTS has been frozen past `pts_stall` — **splice to slate**.
    #[default]
    Splice,
}

impl PacketLivenessState {
    /// Whether this state means the copy thread should emit slate instead of the
    /// source bytes.
    #[must_use]
    pub const fn is_splice(self) -> bool {
        matches!(self, Self::Splice)
    }
}

/// The packet-liveness thresholds for one program, tied to the frame interval
/// `T = 1/fps` and the segment duration `Sd` (ADR-0030 §4).
///
/// Per the ADR: `STALE = 2·T`, `SPLICE = max(4·T, 150 ms)`,
/// `NO_SIGNAL = max(2·Sd, 3 s)`, and the stalled-PTS deadline
/// `pts_stall = splice + 2·T`. Build from the program's `(T, Sd)` with
/// [`PacketLivenessThresholds::from_frame_and_segment`], or supply derived
/// values directly with [`PacketLivenessThresholds::new`].
///
/// `stale`/`splice`/`nosignal` reuse the framestore [`TileThresholds`] ladder so
/// the classification is exactly the same pure function the tile state machine
/// uses; `pts_stall` is the extra stalled-PTS deadline layered on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketLivenessThresholds {
    ladder: TileThresholds,
    pts_stall_ns: i64,
}

impl PacketLivenessThresholds {
    /// 150 ms in nanoseconds — the absolute floor under `SPLICE`.
    const SPLICE_FLOOR_NS: i64 = 150_000_000;
    /// 3 s in nanoseconds — the absolute floor under `NO_SIGNAL`.
    const NOSIGNAL_FLOOR_NS: i64 = 3_000_000_000;

    /// Build the thresholds from the program's frame interval and segment
    /// duration, in nanoseconds, applying the ADR-0030 formula:
    ///
    /// * `stale    = 2 · T`
    /// * `splice   = max(4 · T, 150 ms)`
    /// * `nosignal = max(2 · Sd, 3 s)`
    /// * `pts_stall = splice + 2 · T`
    ///
    /// # Errors
    ///
    /// Returns [`Error::NonPositiveThreshold`] if `frame_interval_ns` or
    /// `segment_ns` is `<= 0`, or [`Error::NonMonotonicThresholds`] if the
    /// derived ladder is not strictly increasing (it cannot be for sane inputs,
    /// but the validating [`TileThresholds::new`] is the single source of
    /// truth). Arithmetic saturates, so absurd inputs cannot overflow.
    pub fn from_frame_and_segment(frame_interval_ns: i64, segment_ns: i64) -> Result<Self> {
        if frame_interval_ns <= 0 {
            return Err(Error::NonPositiveThreshold(frame_interval_ns));
        }
        if segment_ns <= 0 {
            return Err(Error::NonPositiveThreshold(segment_ns));
        }
        let two_t = frame_interval_ns.saturating_mul(2);
        let four_t = frame_interval_ns.saturating_mul(4);
        let stale = two_t;
        let splice = four_t.max(Self::SPLICE_FLOOR_NS);
        let nosignal = segment_ns.saturating_mul(2).max(Self::NOSIGNAL_FLOOR_NS);
        let pts_stall = splice.saturating_add(two_t);
        Self::new(stale, splice, nosignal, pts_stall)
    }

    /// Build directly from already-derived nanosecond values: the `stale`,
    /// `splice`, and `nosignal` ladder points and the `pts_stall` deadline.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NonPositiveThreshold`] / [`Error::NonMonotonicThresholds`]
    /// from [`TileThresholds::new`] if the ladder is non-positive or not strictly
    /// increasing, or [`Error::NonPositiveThreshold`] if `pts_stall_ns <= 0`.
    pub fn new(stale_ns: i64, splice_ns: i64, nosignal_ns: i64, pts_stall_ns: i64) -> Result<Self> {
        let ladder = TileThresholds::new(
            MediaTime::from_nanos(stale_ns),
            MediaTime::from_nanos(splice_ns),
            MediaTime::from_nanos(nosignal_ns),
        )?;
        if pts_stall_ns <= 0 {
            return Err(Error::NonPositiveThreshold(pts_stall_ns));
        }
        Ok(Self {
            ladder,
            pts_stall_ns,
        })
    }

    /// The `STALE` point (`2 · T`): a gap at-or-past this rides the slow-loris
    /// band but does not yet splice.
    #[must_use]
    pub const fn stale_ns(self) -> i64 {
        self.ladder.hold().as_nanos()
    }

    /// The `SPLICE` point (`max(4 · T, 150 ms)`): elapsed at-or-past this is hard
    /// death and splices.
    #[must_use]
    pub const fn splice_ns(self) -> i64 {
        self.ladder.stale().as_nanos()
    }

    /// The `NO_SIGNAL` point (`max(2 · Sd, 3 s)`): the deeper-degraded threshold,
    /// reported for telemetry once the outage is long.
    #[must_use]
    pub const fn nosignal_ns(self) -> i64 {
        self.ladder.nosignal().as_nanos()
    }

    /// The stalled-PTS deadline (`splice + 2 · T`): bytes flowing but max-seen
    /// DTS frozen at-or-past this splices.
    #[must_use]
    pub const fn pts_stall_ns(self) -> i64 {
        self.pts_stall_ns
    }

    /// The underlying tile ladder (`hold = stale`, `stale = splice`,
    /// `nosignal = nosignal`) reused for [`crate::state::classify`].
    #[must_use]
    const fn ladder(self) -> TileThresholds {
        self.ladder
    }
}

/// A single-stream packet-liveness watchdog: a [`TileStore`](crate::TileStore)
/// minus the frame ring.
///
/// Holds two wait-free atoms plus a hard-death flag, and the program's
/// thresholds. A copy thread [`record_packet`](PacketLiveness::record_packet)s
/// every received packet (Release); the clock thread reads
/// [`classify`](PacketLiveness::classify) /
/// [`should_splice`](PacketLiveness::should_splice) every tick (Acquire). One
/// instance per elementary stream — a program holds two (video + audio).
#[derive(Debug)]
pub struct PacketLiveness {
    /// Arrival instant (ns, from the shared monotonic time source) of the most
    /// recent packet, or [`NEVER_RECORDED`]. Release-stamped on every packet.
    last_packet_at_ns: AtomicI64,
    /// Arrival instant (ns) at which the **max-seen DTS last advanced**, or
    /// [`NEVER_RECORDED`]. Stamped only when a packet's DTS exceeds every DTS
    /// seen so far — so a frozen encoder looping a stale DTS is detected even
    /// while bytes keep flowing.
    last_advancing_dts_at_ns: AtomicI64,
    /// The running maximum DTS seen so far (in the stream's own units), or
    /// [`NO_DTS_SEEN`]. Used to decide whether a packet *advances* the timeline.
    max_dts_seen: AtomicI64,
    /// Sticky hard-death flag the caller raises when `read_packet` returns
    /// `Eof`/error. Once set, the stream is dead irrespective of elapsed.
    dead: AtomicBool,
    /// The program's packet-liveness thresholds.
    thresholds: PacketLivenessThresholds,
}

impl PacketLiveness {
    /// Create a watchdog for one elementary stream with the given thresholds.
    ///
    /// Until the first [`record_packet`](PacketLiveness::record_packet), the
    /// stream is treated as dead (it has produced nothing), so
    /// [`should_splice`](PacketLiveness::should_splice) is `true` — failover is
    /// the safe default before any byte arrives.
    #[must_use]
    pub fn new(thresholds: PacketLivenessThresholds) -> Self {
        Self {
            last_packet_at_ns: AtomicI64::new(NEVER_RECORDED),
            last_advancing_dts_at_ns: AtomicI64::new(NEVER_RECORDED),
            max_dts_seen: AtomicI64::new(NO_DTS_SEEN),
            dead: AtomicBool::new(false),
            thresholds,
        }
    }

    /// The configured thresholds.
    #[must_use]
    pub const fn thresholds(&self) -> PacketLivenessThresholds {
        self.thresholds
    }

    /// Record a received packet that arrived at instant `now_ns` (from the
    /// shared monotonic time source — the **arrival** instant, never the packet
    /// PTS) carrying decode timestamp `dts`.
    ///
    /// Release-stamps `last_packet_at_ns` unconditionally (the packet clock).
    /// Additionally, if `dts` **strictly advances** the max-seen DTS, stamps
    /// `last_advancing_dts_at_ns` (the advancing-PTS clock) — a packet whose DTS
    /// does not advance the maximum (B-frame reorder dipping below, or a frozen
    /// encoder repeating a stamp) leaves that clock untouched, so the
    /// stalled-PTS signal keeps running.
    ///
    /// Wait-free; called on the source-paced copy thread, never the clock
    /// thread.
    pub fn record_packet(&self, now_ns: i64, dts: i64) {
        // Advancing-DTS check first: stamp the advancing clock *before* the
        // packet clock is made visible, so a reader that sees a fresh
        // `last_packet_at_ns` also sees an up-to-date advancing stamp (never a
        // newer packet with a stale advancing instant for the same write).
        let prev_max = self.max_dts_seen.load(Ordering::Relaxed);
        if dts > prev_max {
            self.max_dts_seen.store(dts, Ordering::Relaxed);
            self.last_advancing_dts_at_ns
                .store(now_ns, Ordering::Release);
        }
        self.last_packet_at_ns.store(now_ns, Ordering::Release);
    }

    /// Mark the stream hard-dead because `read_packet` returned end-of-file.
    ///
    /// Sticky: once set, [`should_splice`](PacketLiveness::should_splice) is
    /// `true` regardless of elapsed. The recovery path constructs a fresh
    /// watchdog after a successful reconnect.
    pub fn mark_eof(&self) {
        self.dead.store(true, Ordering::Release);
    }

    /// Mark the stream hard-dead because `read_packet` returned an error.
    ///
    /// Identical sticky semantics to [`mark_eof`](PacketLiveness::mark_eof);
    /// kept distinct for call-site clarity.
    pub fn mark_error(&self) {
        self.dead.store(true, Ordering::Release);
    }

    /// Whether the caller has raised the hard-death flag.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Elapsed ns since the last packet as of `now_ns`, or `None` if no packet
    /// has ever been recorded.
    ///
    /// Saturating and clamped non-negative: a `now_ns` earlier than the stamp
    /// (clock skew / a stale Acquire read of a *newer* stamp) yields `0`, never
    /// a negative elapsed — the monotonic-guard case. A *staler* observed stamp
    /// (the wait-free hazard) yields a **larger** elapsed, which is the
    /// fail-safe direction (biases toward splice).
    #[must_use]
    fn elapsed_since_packet_ns(&self, now_ns: i64) -> Option<i64> {
        let last = self.last_packet_at_ns.load(Ordering::Acquire);
        if last == NEVER_RECORDED {
            return None;
        }
        Some(now_ns.saturating_sub(last).max(0))
    }

    /// Elapsed ns since the max-seen DTS last advanced as of `now_ns`, or `None`
    /// if no DTS has ever advanced.
    ///
    /// Same saturating / non-negative clamp as
    /// [`elapsed_since_packet_ns`](PacketLiveness::elapsed_since_packet_ns).
    #[must_use]
    fn elapsed_since_advancing_dts_ns(&self, now_ns: i64) -> Option<i64> {
        let last = self.last_advancing_dts_at_ns.load(Ordering::Acquire);
        if last == NEVER_RECORDED {
            return None;
        }
        Some(now_ns.saturating_sub(last).max(0))
    }

    /// Classify the stream's copy-vs-splice state as of `now_ns`.
    ///
    /// Pure function of `now_ns`, the two stamps, the max-seen DTS, the
    /// hard-death flag, and the thresholds — reusing the tile-ladder
    /// [`crate::state::classify`] for the elapsed-since-packet axis:
    ///
    /// 1. **Hard death** (`is_dead`, no packet yet, or the ladder reports
    ///    `Reconnecting`/`NoSignal` ⇒ elapsed `>= splice`) ⇒
    ///    [`Splice`](PacketLivenessState::Splice).
    /// 2. **Stalled PTS** (bytes flowing but advancing-DTS elapsed `>=
    ///    pts_stall`) ⇒ [`Splice`](PacketLivenessState::Splice).
    /// 3. Otherwise the ladder's `Live`/`Stale` maps straight through (the
    ///    slow-loris band rides [`Stale`](PacketLivenessState::Stale), never
    ///    splices).
    #[must_use]
    pub fn classify(&self, now_ns: i64) -> PacketLivenessState {
        // Signal 1a: explicit hard death.
        if self.is_dead() {
            return PacketLivenessState::Splice;
        }
        // Signal 1b: never produced a packet ⇒ dead by default.
        let Some(packet_elapsed) = self.elapsed_since_packet_ns(now_ns) else {
            return PacketLivenessState::Splice;
        };
        // Reuse the tile ladder: hold=stale, stale=splice, nosignal=nosignal.
        // `Reconnecting`/`NoSignal` both mean elapsed >= splice ⇒ hard death.
        let ladder_state = classify(
            MediaTime::from_nanos(packet_elapsed),
            self.thresholds.ladder(),
        );
        match ladder_state {
            SourceState::Reconnecting | SourceState::NoSignal => {
                return PacketLivenessState::Splice;
            }
            SourceState::Live | SourceState::Stale => {}
            // `SourceState` is `#[non_exhaustive]`; any future deeper-degraded
            // state is at-or-past the splice point ⇒ splice (fail-safe).
            _ => return PacketLivenessState::Splice,
        }
        // Signal 3: bytes flowing but max-DTS frozen past pts_stall. Only checked
        // on the still-live packet path; if no DTS has ever advanced while
        // packets flow, the stall clock has not started, so the stream rides the
        // packet ladder until either a DTS advances or the packet clock itself
        // crosses splice.
        if let Some(dts_elapsed) = self.elapsed_since_advancing_dts_ns(now_ns) {
            if dts_elapsed >= self.thresholds.pts_stall_ns {
                return PacketLivenessState::Splice;
            }
        }
        // Signal 2: slow-loris / stutter rides the ladder's Live/Stale band.
        match ladder_state {
            SourceState::Stale => PacketLivenessState::Stale,
            // Live (and, defensively, anything not already returned above).
            _ => PacketLivenessState::Live,
        }
    }

    /// Whether the copy thread should splice to slate as of `now_ns`.
    ///
    /// Equivalent to `self.classify(now_ns).is_splice()`. Wait-free; **fails
    /// safe** under a stale Acquire read (an older observed stamp ⇒ larger
    /// elapsed ⇒ biases toward `true`, never a false `false`).
    #[must_use]
    pub fn should_splice(&self, now_ns: i64) -> bool {
        self.classify(now_ns).is_splice()
    }
}
