//! RTCP **Sender Report** building (RFC 3550 §6.4.1) stamped from the
//! outbound presentation epoch (ADR-M010, DEV-C1) — pure integer math, always
//! compiled, no native dependency.
//!
//! An SR binds a wall-clock instant (the 64-bit NTP timestamp) to the media
//! clock (the 32-bit RTP timestamp) so receivers can map RTP time onto a
//! shared wall timeline. Multiview stamps that pair from the **same**
//! [`WallClockRef`] epoch the control WS publishes and the HLS
//! `EXT-X-PROGRAM-DATE-TIME` tags are derived from — one anchor, every
//! surface agrees.
//!
//! ## What consumes this
//!
//! [`SrStamper`] is the seam the RTSP serving layer consumes (carried by
//! [`RtspServerSink`](crate::rtsp_server::RtspServerSink)). The actual RTCP
//! emission today lives **inside** the feature-gated `gst-rtsp-server`
//! serving path, whose `rtpbin` generates its own pipeline-clock SRs; see
//! [`crate::rtsp_server`] for the precise wired-vs-seam boundary.
//!
//! All arithmetic is exact integer (`i128` intermediates) — never float
//! (invariant #3). Counts (`packet_count`/`octet_count`) are the *sender's*
//! RTP-layer totals and are supplied by the transport that owns them; this
//! module never fabricates them.

use multiview_core::time::{rescale, Rational};
use multiview_core::wallclock::WallClockRef;

use crate::epoch::SharedEpoch;

/// Seconds between the NTP era origin (1900-01-01) and the Unix epoch
/// (1970-01-01): 70 years including 17 leap days.
const NTP_UNIX_OFFSET_SECS: i64 = 2_208_988_800;

/// A 64-bit NTP timestamp (RFC 3550 §4): whole seconds since 1900-01-01 in
/// the high word, a 2^-32-seconds fraction in the low word. Both wrap modulo
/// 2^32 (the era is the receiver's concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NtpTimestamp {
    /// Whole seconds since 1900-01-01T00:00:00Z, modulo 2^32.
    pub seconds: u32,
    /// The fractional second in 2^-32 units.
    pub fraction: u32,
}

impl NtpTimestamp {
    /// Convert a Unix instant (integer ns past 1970-01-01, UTC) to the NTP
    /// timestamp: Euclidean-split into whole seconds + sub-second ns, shift
    /// by the 1900↔1970 era offset (seconds wrap modulo 2^32), and scale the
    /// sub-second part to 2^-32 units exactly in `i128` (rounding half away
    /// from zero — the sub-second part is `< 1 s`, so the rounded fraction
    /// never carries into the seconds word).
    #[must_use]
    pub fn from_unix_ns(unix_ns: i64) -> Self {
        let unix_secs = unix_ns.div_euclid(1_000_000_000);
        let subsec_ns = unix_ns.rem_euclid(1_000_000_000); // [0, 1e9)
        let ntp_secs = unix_secs.saturating_add(NTP_UNIX_OFFSET_SECS);
        // Modulo-2^32 wrap (RFC 3550 carries only the low 64 bits).
        let seconds = u32::try_from(ntp_secs.rem_euclid(1_i64 << 32)).unwrap_or(0);
        // fraction = round(subsec_ns * 2^32 / 1e9), exact in i128. The result
        // is at most round(999_999_999 * 2^32 / 1e9) = 4_294_967_292 < 2^32.
        let scaled = i128::from(subsec_ns) << 32;
        let fraction128 = (scaled + 500_000_000) / 1_000_000_000;
        let fraction = u32::try_from(fraction128).unwrap_or(u32::MAX);
        Self { seconds, fraction }
    }
}

/// The RTP timestamp the epoch's media clock assigns to a wall instant:
/// `rtp = base + rescale(media_at(wall) → clock_rate)`, modulo 2^32.
///
/// `epoch` carries media units in its own rate (the canonical outbound epoch
/// uses output-PTS nanoseconds); the position is rescaled exactly into
/// `clock_rate` ticks (e.g. 90 kHz for video) and offset by the stream's
/// random RTP base. Exact integer (`i128` via [`rescale`]) — never float.
#[must_use]
pub fn rtp_timestamp_at(epoch: &WallClockRef, wall_ns: i64, clock_rate: u32, rtp_base: u32) -> u32 {
    let media_units = epoch.media_at(wall_ns);
    // Rescale from the epoch's seconds-per-unit timebase into the RTP clock's
    // 1/clock_rate-seconds timebase.
    let epoch_timebase = Rational::new(epoch.rate.den, epoch.rate.num);
    let rtp_ticks = rescale(
        media_units,
        epoch_timebase,
        Rational::new(1, i64::from(clock_rate)),
    );
    // Modulo-2^32 wrap onto the RTP timestamp ring, then the base offset.
    let wrapped = u32::try_from(rtp_ticks.rem_euclid(1_i64 << 32)).unwrap_or(0);
    rtp_base.wrapping_add(wrapped)
}

/// One RTCP Sender Report (RFC 3550 §6.4.1) with no report blocks: the
/// five-word sender-info body every Multiview RTSP session stamps from the
/// program epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderReport {
    /// The sender's synchronization source identifier.
    pub ssrc: u32,
    /// The wall instant of this report (NTP format, from the epoch's
    /// disciplined wall clock).
    pub ntp: NtpTimestamp,
    /// The media-clock instant corresponding to `ntp` (same epoch, same
    /// instant — the pair receivers interpolate between).
    pub rtp_timestamp: u32,
    /// The sender's total RTP packet count (transport-owned).
    pub packet_count: u32,
    /// The sender's total RTP payload octet count (transport-owned).
    pub octet_count: u32,
}

impl SenderReport {
    /// Serialize to the exact 28-byte wire form: `V=2 P=0 RC=0`, `PT=200`,
    /// length `6` (32-bit words minus one), then SSRC, NTP (2 words), RTP
    /// timestamp, packet count, octet count — all big-endian.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 28] {
        let mut out = [0u8; 28];
        let words: [[u8; 4]; 7] = [
            [0x80, 0xC8, 0x00, 0x06],
            self.ssrc.to_be_bytes(),
            self.ntp.seconds.to_be_bytes(),
            self.ntp.fraction.to_be_bytes(),
            self.rtp_timestamp.to_be_bytes(),
            self.packet_count.to_be_bytes(),
            self.octet_count.to_be_bytes(),
        ];
        let mut offset = 0usize;
        for word in words {
            for byte in word {
                if let Some(slot) = out.get_mut(offset) {
                    *slot = byte;
                }
                offset = offset.saturating_add(1);
            }
        }
        out
    }
}

/// The epoch-fed SR stamper: binds a [`SharedEpoch`] cell to one RTP stream's
/// identity (`ssrc`, `clock_rate`, random `rtp_base`) and builds
/// [`SenderReport`]s for the transport on demand.
///
/// Cheap to clone (the cell is `Arc`-backed); read-only over the epoch —
/// it can never influence the sampler, the engine, or the tick loop.
#[derive(Debug, Clone)]
pub struct SrStamper {
    epoch: SharedEpoch,
    clock_rate: u32,
    ssrc: u32,
    rtp_base: u32,
}

impl SrStamper {
    /// Bind a stamper to the program's epoch cell and the stream identity.
    #[must_use]
    pub const fn new(epoch: SharedEpoch, clock_rate: u32, ssrc: u32, rtp_base: u32) -> Self {
        Self {
            epoch,
            clock_rate,
            ssrc,
            rtp_base,
        }
    }

    /// Build the SR for the wall instant `wall_ns` with the transport's RTP
    /// totals, or `None` while no epoch has been published yet (a fabricated
    /// NTP↔RTP pair is worse than none — receivers would sync to fiction).
    #[must_use]
    pub fn report(
        &self,
        wall_ns: i64,
        packet_count: u32,
        octet_count: u32,
    ) -> Option<SenderReport> {
        let epoch = self.epoch.get()?;
        Some(SenderReport {
            ssrc: self.ssrc,
            ntp: NtpTimestamp::from_unix_ns(wall_ns),
            rtp_timestamp: rtp_timestamp_at(&epoch, wall_ns, self.clock_rate, self.rtp_base),
            packet_count,
            octet_count,
        })
    }
}
