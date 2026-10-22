//! The SAP **announce schedule** and **packet builders** (RFC 2974 §3/§5/§6;
//! ADR-0041 §5, brief §3) — pure logic, no sockets.
//!
//! An announcer re-sends each session's SDP on a **≥ 30 s** base cadence (the
//! Dante/AES67 interop default; the RFC bandwidth-fair timer is a future option)
//! with **±1/3 jitter** so many announcers on a group de-synchronise rather than
//! pulse together. [`AnnounceSchedule::next_delay`] takes an externally-supplied
//! random `sample` (the transport's RNG) and returns a delay in
//! `[2/3·base, 4/3·base)`; keeping the RNG out of this module leaves the schedule
//! deterministic and testable.
//!
//! [`announcement`] and [`deletion`] build the `T=0` / `T=1` [`SapPacket`]s from a
//! session's stable non-zero [`stable_hash`], origin, and SDP — carrying the
//! explicit `application/sdp` payload-type. The announcer emits a deletion as a
//! courtesy on teardown; inbound deletions are ignored elsewhere (ADR-0041 §8).
//!
//! This is off the output clock and cannot pace it (inv #1): the announce timer
//! is independent of the per-tick output loop.

use std::net::IpAddr;
use std::num::NonZeroU16;
use std::time::Duration;

use super::packet::{SapMessageType, SapPacket, SDP_MIME_TYPE};

/// The minimum announce cadence — the ≥ 30 s interop floor (ADR-0041 §5). A
/// shorter request is clamped up to this.
pub const MIN_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// Numerator of the jitter window fraction (window width = 2/3 of the base).
const JITTER_WINDOW_NUM: u32 = 2;
/// Denominator of the jitter window fraction.
const JITTER_WINDOW_DEN: u32 = 3;

/// The pure announce cadence: a floored base interval that yields a fresh
/// ±1/3-jittered delay per cycle.
#[derive(Debug, Clone, Copy)]
pub struct AnnounceSchedule {
    base: Duration,
}

impl AnnounceSchedule {
    /// Create a schedule with the given base cadence, clamped up to
    /// [`MIN_ANNOUNCE_INTERVAL`].
    #[must_use]
    pub fn new(base: Duration) -> Self {
        Self {
            base: base.max(MIN_ANNOUNCE_INTERVAL),
        }
    }

    /// The (floored) base cadence this schedule jitters around.
    #[must_use]
    pub fn base_interval(&self) -> Duration {
        self.base
    }

    /// The next delay to wait before re-announcing, jittered ±1/3 around the base
    /// from an externally-supplied uniform `sample`: the result lies in
    /// `[2/3·base, 4/3·base)` (`sample = 0` → the low endpoint; `sample = u64::MAX`
    /// → just under the high endpoint).
    #[must_use]
    pub fn next_delay(&self, sample: u64) -> Duration {
        jittered_interval(self.base, sample)
    }
}

/// Map a uniform `sample` to a delay in `[2/3·base, 4/3·base)` (the RFC 2974 ±1/3
/// jitter, brief §3), using integer nanosecond math so no float rounding or
/// lossy cast is involved.
fn jittered_interval(base: Duration, sample: u64) -> Duration {
    // low endpoint = 2/3·base; the jitter window is the same 2/3·base wide, so
    // the result spans [2/3·base, 2/3·base + 2/3·base) = [2/3·base, 4/3·base).
    let two_thirds = base * JITTER_WINDOW_NUM / JITTER_WINDOW_DEN;
    let width_ns = two_thirds.as_nanos();
    // offset = width · sample / 2^64 ∈ [0, width): scales the full u64 sample
    // range into the window without a modulo bias worth caring about here.
    let two_pow_64 = u128::from(u64::MAX) + 1;
    let offset_ns = width_ns.saturating_mul(u128::from(sample)) / two_pow_64;
    let next_ns = width_ns.saturating_add(offset_ns);
    Duration::from_nanos(u64::try_from(next_ns).unwrap_or(u64::MAX))
}

/// Build a `T=0` announcement packet for a session (its stable [`stable_hash`],
/// originating source, and opaque SDP body), carrying the explicit
/// `application/sdp` payload-type.
#[must_use]
pub fn announcement(hash: NonZeroU16, origin: IpAddr, sdp: Vec<u8>) -> SapPacket {
    SapPacket {
        message_type: SapMessageType::Announcement,
        msg_id_hash: hash,
        origin,
        payload_type: Some(SDP_MIME_TYPE.to_owned()),
        payload: sdp,
    }
}

/// Build a courtesy `T=1` deletion packet for a previously-announced session,
/// carrying the same hash/origin/SDP so a receiver can identify it. (Receivers —
/// including Multiview — ignore inbound deletions against tracked sessions per
/// ADR-0041 §8; this is emitted only on our own teardown.)
#[must_use]
pub fn deletion(hash: NonZeroU16, origin: IpAddr, sdp: Vec<u8>) -> SapPacket {
    SapPacket {
        message_type: SapMessageType::Deletion,
        msg_id_hash: hash,
        origin,
        payload_type: Some(SDP_MIME_TYPE.to_owned()),
        payload: sdp,
    }
}

/// FNV-1a/64 offset basis (the defined algorithm's fixed seed).
const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a/64 prime.
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A stable, **non-zero** 16-bit message-id hash of an SDP body (RFC 2974 §8): the
/// same SDP always yields the same hash (so a receiver de-duplicates), and a
/// changed SDP yields a different one (so a modification is detectable). Zero (the
/// reserved value) is never produced — the [`NonZeroU16`] return type enforces it.
///
/// Uses **FNV-1a**, a small, fully-defined algorithm, so the mapping is stable
/// across toolchain versions, targets, and restarts (P2-F4). `DefaultHasher` is
/// explicitly *not* a stable cross-version contract — a toolchain bump could
/// change the hash for identical SDP, and a receiver would then see a restarted
/// announcer's unchanged sessions as new ones (duplicate sessions until timeout),
/// so it must never back this identifier.
#[must_use]
pub fn stable_hash(sdp: &[u8]) -> NonZeroU16 {
    let mut hash = FNV1A_OFFSET_BASIS;
    for &byte in sdp {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A_PRIME);
    }
    // Fold the 64-bit digest into 16 bits, then force non-zero (0 is reserved).
    let folded = hash ^ (hash >> 16) ^ (hash >> 32) ^ (hash >> 48);
    let low = u16::try_from(folded & 0xFFFF).unwrap_or(1);
    NonZeroU16::new(low).unwrap_or(NonZeroU16::MIN)
}
