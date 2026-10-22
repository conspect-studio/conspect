//! Direct librist (VSF TR-06, BSD-2-Clause) FFI leaf for RIST **link
//! statistics** — ADR-0095 Tier-1 / RIST-5.
//!
//! ## Why this crate exists
//!
//! Multiview's Tier-0 RIST ingest/egress rides `FFmpeg`'s `rist://` protocol
//! (ADR-0095): single-link Simple/Main + PSK, the LGPL-clean baseline. But that
//! protocol **structurally cannot report link statistics** — `libavformat`'s
//! `librist.c` registers no `rist_stats_callback_set` and exposes no stats
//! `AVOption` (confirmed empirically: `ffmpeg -h protocol=rist` lists only
//! profile/buffer/fifo/pkt/secret/encryption options). So a RIST link's health —
//! retransmits, RTT, link quality, bandwidth, lost/recovered — is **invisible**
//! through the `FFmpeg` transport, because `FFmpeg` owns that librist context
//! privately and there is no librist API to observe a context another library
//! created.
//!
//! Obtaining stats therefore requires a librist context **we** own, with
//! `rist_stats_callback_set` wired on it. This crate is that owned FFI leaf
//! (ADR-0028 own-the-binding pattern, the same runtime-load model as
//! `multiview-ndi-sys`):
//!
//! * The **C-ABI stats decode** ([`decode_stats`], [`raw`]) — pure, offline,
//!   no native dep — lowers a librist `rist_stats` blob into the neutral
//!   [`multiview_telemetry::rist::RistLinkSample`] the metrics surface consumes.
//! * The **`session` feature** adds the runtime-loaded librist sender session
//!   ([`session`]) that owns a `rist_ctx`, registers the stats callback, and
//!   publishes decoded samples — a real direct-librist egress transport with
//!   stats, the leaf-sized Tier-1 path for an `Output::Rist`.
//!
//! ## The honest `FFmpeg`-vs-librist boundary (ADR-0095 adversarial finding)
//!
//! Stats cannot be bolted onto `FFmpeg`'s `rist://` socket. The **egress** sender
//! is leaf-sized: a direct-librist sender is just an alternative push sink that
//! consumes the same encoded MPEG-TS packets (inv #7) and owns a context with
//! stats — it replaces no shared data-plane transport. The **ingress** receiver
//! with stats is a larger change (owning the librist receive+demux loop, a new
//! `Source`), so it is **not** built here; the [`decode`](decode_stats) handles
//! the receiver-flow shape (so the model is complete and ready) and the
//! receiver transport is documented as the Tier-2-shaped follow-up. We never
//! fabricate stats: a number is surfaced only when a librist context we own
//! produced it.
//!
//! ## FFI safety (safety rules §4)
//!
//! All raw `dlopen`/`dlsym` and every C-struct deref live in this crate; it is
//! the sole `unsafe` boundary so `multiview-output` stays `forbid(unsafe_code)`.
//! Every `unsafe` block carries a `// SAFETY:` note. The stats callback runs on
//! librist's own thread and only ever publishes to a bounded drop-oldest channel
//! — it never blocks and never unwinds across the FFI boundary (inv #10).

#![warn(missing_docs)]

pub mod raw;

#[cfg(feature = "session")]
pub mod session;

use multiview_telemetry::rist::{RistLinkRole, RistLinkSample};
use raw::{RawStats, RawStatsType};

/// An error decoding a librist `rist_stats` blob into a [`RistLinkSample`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The `stats_type` discriminant was neither sender-peer (0) nor
    /// receiver-flow (1) — a newer librist or a corrupt blob. The raw value is
    /// carried so the caller can log it; we never guess the union arm.
    #[error("librist reported an unknown rist_stats_type discriminant: {0}")]
    UnknownStatsType(i32),
}

/// Decode a librist [`RawStats`] container into the neutral telemetry
/// [`RistLinkSample`], reading the union arm the `stats_type` discriminant
/// selects (never the wrong arm).
///
/// `link_id` is the configured RIST source/output id this link belongs to;
/// `since_ns` is the engine-monotonic nanosecond timestamp the link first
/// reported stats (stamped by the caller, not librist).
///
/// # Errors
/// [`DecodeError::UnknownStatsType`] if the discriminant is not a known arm.
pub fn decode_stats(
    stats: &RawStats,
    link_id: impl Into<String>,
    since_ns: i64,
) -> Result<RistLinkSample, DecodeError> {
    match stats.stats_type {
        RawStatsType::SENDER_PEER => {
            // SAFETY: the C union's active arm is selected by `stats_type`, which
            // we have just matched as SENDER_PEER; reading `sender_peer` is the
            // arm librist populated. `RawStatsSenderPeer` is `Copy` plain-old-data
            // (no pointers), so the read is a value copy with no aliasing concern.
            #[allow(unsafe_code)]
            let peer = unsafe { stats.stats.sender_peer };
            Ok(RistLinkSample {
                link_id: link_id.into(),
                role: RistLinkRole::Sender,
                // The sender-peer struct has no flow id; it is a receiver concept.
                flow_id: 0,
                cname: c_array_to_string(&peer.cname),
                // A sender peer is, by definition, a single peer link.
                peer_count: 1,
                rtt_ms: peer.rtt,
                quality: peer.quality,
                bandwidth_bps: usize_to_u64(peer.bandwidth),
                retry_bandwidth_bps: usize_to_u64(peer.retry_bandwidth),
                sent: peer.sent,
                received: peer.received,
                retransmitted: peer.retransmitted,
                // Sender-side has no lost/recovered counters in librist's struct.
                lost: 0,
                recovered: 0,
                since: since_ns,
            })
        }
        RawStatsType::RECEIVER_FLOW => {
            // SAFETY: `stats_type` matched RECEIVER_FLOW, so `receiver_flow` is
            // the arm librist populated. `RawStatsReceiverFlow` is `Copy` POD; the
            // read is a value copy.
            #[allow(unsafe_code)]
            let flow = unsafe { stats.stats.receiver_flow };
            Ok(RistLinkSample {
                link_id: link_id.into(),
                role: RistLinkRole::Receiver,
                flow_id: flow.flow_id,
                cname: c_array_to_string(&flow.cname),
                peer_count: flow.peer_count,
                rtt_ms: flow.rtt,
                quality: flow.quality,
                bandwidth_bps: usize_to_u64(flow.bandwidth),
                retry_bandwidth_bps: usize_to_u64(flow.retry_bandwidth),
                sent: flow.sent,
                received: flow.received,
                // The receiver-flow struct has no single "retransmitted" field;
                // its ARQ work is the `recovered`/`lost` pair.
                retransmitted: 0,
                lost: u64::from(flow.lost),
                recovered: u64::from(flow.recovered),
                since: since_ns,
            })
        }
        RawStatsType(other) => Err(DecodeError::UnknownStatsType(other)),
    }
}

/// Read a fixed-size C `char` array as a Rust `String`, stopping at the first NUL
/// and bounded by the buffer length (a buffer with no terminator is read in full,
/// never past its end). Non-UTF-8 bytes are replaced (lossy) so a garbled cname
/// can never panic the telemetry path.
fn c_array_to_string(buf: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .map(|&c| raw::u8_from_cchar(c))
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Widen a C `size_t` (`usize`) to the `u64` the telemetry sample carries,
/// without a lossy `as` cast (the workspace denies `as_conversions`). On every
/// supported target `usize <= u64`, so this is always exact.
fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw::{cchar_from_u8, RawStatsSenderPeer, RIST_MAX_STRING_SHORT};

    #[test]
    fn c_array_stops_at_nul() {
        let mut buf = [cchar_from_u8(0); RIST_MAX_STRING_SHORT];
        buf[0] = cchar_from_u8(b'h');
        buf[1] = cchar_from_u8(b'i');
        // buf[2] stays NUL
        assert_eq!(c_array_to_string(&buf), "hi");
    }

    #[test]
    fn usize_to_u64_is_exact() {
        assert_eq!(usize_to_u64(0), 0);
        assert_eq!(usize_to_u64(12_000_000), 12_000_000);
    }

    #[test]
    fn sender_peer_decodes_role_and_single_peer() {
        let raw = RawStats::sender_peer(RawStatsSenderPeer::zeroed());
        let s = decode_stats(&raw, "l", 5).unwrap();
        assert_eq!(s.role, RistLinkRole::Sender);
        assert_eq!(s.peer_count, 1);
        assert_eq!(s.since, 5);
    }
}
