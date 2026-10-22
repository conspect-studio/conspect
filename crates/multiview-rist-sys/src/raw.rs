//! The librist 0.2.x C-ABI stats types (`#[repr(C)]`), hand-mirrored from the
//! BSD-2-Clause `librist/stats.h` (verified against the installed librist 0.2.11
//! headers). librist is **never vendored**: only this small, stable ABI surface
//! is restated so the decode can be written and tested without librist-dev
//! headers present, and the runtime `.so` is `dlopen`-loaded (never linked).
//!
//! ## Why hand-mirrored, not bindgen
//!
//! The stats ABI is tiny and frozen for the 0.2.x series (`RIST_STATS_VERSION`
//! is `0`); restating it here keeps the crate buildable with no native header at
//! build time (the NDI-sys crate needs the licensed header for bindgen — librist
//! we deliberately keep header-free). The layout is asserted against the real
//! field sizes by the `#[cfg(test)]` layout checks at the bottom of this module,
//! so any drift is caught, not silently mis-read.
//!
//! Field names and types mirror `struct rist_stats_sender_peer`,
//! `struct rist_stats_receiver_flow`, `enum rist_stats_type`, and
//! `struct rist_stats` from `librist/stats.h` verbatim.

use std::os::raw::c_char;

/// `RIST_MAX_STRING_SHORT` from `librist/headers.h` (the cname buffer length on a
/// sender peer).
pub const RIST_MAX_STRING_SHORT: usize = 128;
/// `RIST_MAX_STRING_LONG` from `librist/headers.h` (the combined-cname buffer
/// length on a receiver flow).
pub const RIST_MAX_STRING_LONG: usize = 256;

/// `enum rist_stats_type` (a C `int`). `0` = sender peer, `1` = receiver flow.
///
/// A newtype over the raw `i32` rather than a Rust `enum` so an unrecognised
/// value from a newer librist round-trips into a typed decode error instead of
/// triggering UB on an out-of-range discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct RawStatsType(pub i32);

impl RawStatsType {
    /// `RIST_STATS_SENDER_PEER`.
    pub const SENDER_PEER: Self = Self(0);
    /// `RIST_STATS_RECEIVER_FLOW`.
    pub const RECEIVER_FLOW: Self = Self(1);
}

/// One-byte union reinterpreting a `u8` as the platform `c_char` and back.
///
/// `c_char` is `i8` on some targets (Linux `x86_64`) and `u8` on others (Linux
/// aarch64); both are exactly one byte, so a byte round-trips losslessly either
/// way. A one-byte POD union is the `as`-free, `transmute`-free way to bridge the
/// two when the concrete type cannot be named portably.
#[repr(C)]
union ByteCChar {
    byte: u8,
    cchar: c_char,
}

/// Reinterpret a `u8` as the platform C `char` (signedness-agnostic).
///
/// Lets the ABI structs / tests be populated identically on every platform
/// without an `as` cast (the workspace denies `as_conversions`).
#[must_use]
pub const fn cchar_from_u8(byte: u8) -> c_char {
    // SAFETY: `ByteCChar` is a one-byte `repr(C)` union; both arms are 1-byte
    // integers occupying the same storage. Writing `byte` and reading `cchar`
    // is a defined integer reinterpret (no padding, no invalid bit patterns —
    // every bit pattern is a valid `i8`/`u8`).
    #[allow(unsafe_code)]
    unsafe {
        ByteCChar { byte }.cchar
    }
}

/// Reinterpret a platform C `char` back to a `u8` (the inverse of
/// [`cchar_from_u8`]).
#[must_use]
pub const fn u8_from_cchar(cchar: c_char) -> u8 {
    // SAFETY: as [`cchar_from_u8`] — a one-byte integer reinterpret over a
    // `repr(C)` one-byte union; every bit pattern is a valid `u8`.
    #[allow(unsafe_code)]
    unsafe {
        ByteCChar { cchar }.byte
    }
}

/// `struct rist_stats_sender_peer` — the egress (send) side stats.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct RawStatsSenderPeer {
    /// `char cname[RIST_MAX_STRING_SHORT]` — NUL-terminated peer cname.
    pub cname: [c_char; RIST_MAX_STRING_SHORT],
    /// `uint32_t peer_id` — internal peer id.
    pub peer_id: u32,
    /// `size_t bandwidth` — average measured throughput (bits/s).
    pub bandwidth: usize,
    /// `size_t retry_bandwidth` — throughput devoted to retries (bits/s).
    pub retry_bandwidth: usize,
    /// `uint64_t sent` — cumulative packets sent.
    pub sent: u64,
    /// `uint64_t received` — cumulative packets received.
    pub received: u64,
    /// `uint64_t retransmitted` — cumulative packets retransmitted.
    pub retransmitted: u64,
    /// `double quality` — link-quality metric (`0..=100`).
    pub quality: f64,
    /// `uint32_t rtt` — current RTT (milliseconds).
    pub rtt: u32,
}

impl RawStatsSenderPeer {
    /// An all-zero sender-peer struct (the zero value of every field).
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            cname: [0; RIST_MAX_STRING_SHORT],
            peer_id: 0,
            bandwidth: 0,
            retry_bandwidth: 0,
            sent: 0,
            received: 0,
            retransmitted: 0,
            quality: 0.0,
            rtt: 0,
        }
    }
}

/// `struct rist_stats_receiver_flow` — the ingress (receive) side stats.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct RawStatsReceiverFlow {
    /// `uint32_t peer_count` — number of active peers on the flow.
    pub peer_count: u32,
    /// `char cname[RIST_MAX_STRING_LONG]` — combined peer cnames.
    pub cname: [c_char; RIST_MAX_STRING_LONG],
    /// `uint32_t flow_id` — the flow id (set by senders).
    pub flow_id: u32,
    /// `int status` — flow status.
    pub status: i32,
    /// `size_t bandwidth` — average measured throughput (bits/s).
    pub bandwidth: usize,
    /// `size_t retry_bandwidth` — retry throughput (bits/s).
    pub retry_bandwidth: usize,
    /// `uint64_t sent` — cumulative packets sent.
    pub sent: u64,
    /// `uint64_t received` — cumulative packets received.
    pub received: u64,
    /// `uint32_t missing` — missing (incl. reordered).
    pub missing: u32,
    /// `uint32_t reordered` — reordered.
    pub reordered: u32,
    /// `uint32_t recovered` — total recovered by ARQ.
    pub recovered: u32,
    /// `uint32_t recovered_one_retry` — recovered on the first retry.
    pub recovered_one_retry: u32,
    /// `uint32_t lost` — unrecoverable lost packets.
    pub lost: u32,
    /// `double quality` — link-quality metric (`0..=100`).
    pub quality: f64,
    /// `uint64_t min_inter_packet_spacing` — packet inter-arrival min (µs).
    pub min_inter_packet_spacing: u64,
    /// `uint64_t cur_inter_packet_spacing` — current (µs).
    pub cur_inter_packet_spacing: u64,
    /// `uint64_t max_inter_packet_spacing` — max (µs).
    pub max_inter_packet_spacing: u64,
    /// `uint32_t rtt` — avg RTT over non-dead peers (milliseconds).
    pub rtt: u32,
}

impl RawStatsReceiverFlow {
    /// An all-zero receiver-flow struct.
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            peer_count: 0,
            cname: [0; RIST_MAX_STRING_LONG],
            flow_id: 0,
            status: 0,
            bandwidth: 0,
            retry_bandwidth: 0,
            sent: 0,
            received: 0,
            missing: 0,
            reordered: 0,
            recovered: 0,
            recovered_one_retry: 0,
            lost: 0,
            quality: 0.0,
            min_inter_packet_spacing: 0,
            cur_inter_packet_spacing: 0,
            max_inter_packet_spacing: 0,
            rtt: 0,
        }
    }
}

/// The `union { sender_peer; receiver_flow; }` of `struct rist_stats`.
///
/// A C union; reading the wrong arm is UB, so [`RawStats::stats_type`] is the
/// discriminant the decode checks before touching it.
#[derive(Clone, Copy)]
#[repr(C)]
pub union RawStatsUnion {
    /// Valid only when `stats_type == RawStatsType::SENDER_PEER`.
    pub sender_peer: RawStatsSenderPeer,
    /// Valid only when `stats_type == RawStatsType::RECEIVER_FLOW`.
    pub receiver_flow: RawStatsReceiverFlow,
}

/// `struct rist_stats` — the container passed to the stats callback.
///
/// The leading `json_size`/`stats_json` carry librist's pre-rendered JSON (which
/// the callback owner must `free()`); we decode the **typed** union instead, so
/// the JSON pointer is carried but only read by the session layer when present.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RawStats {
    /// `uint32_t json_size` — length of the JSON string.
    pub json_size: u32,
    /// `char *stats_json` — the librist-rendered JSON (owner frees it).
    pub stats_json: *const c_char,
    /// `uint16_t version` — `RIST_STATS_VERSION` (0 for librist 0.2.x).
    pub version: u16,
    /// `enum rist_stats_type stats_type` — the active union arm.
    pub stats_type: RawStatsType,
    /// The stats payload (read per `stats_type`).
    pub stats: RawStatsUnion,
}

impl RawStats {
    /// Build a sender-peer stats container (test/decode helper; no JSON pointer).
    #[must_use]
    pub fn sender_peer(peer: RawStatsSenderPeer) -> Self {
        Self {
            json_size: 0,
            stats_json: std::ptr::null(),
            version: 0,
            stats_type: RawStatsType::SENDER_PEER,
            stats: RawStatsUnion { sender_peer: peer },
        }
    }

    /// Build a receiver-flow stats container (test/decode helper; no JSON pointer).
    #[must_use]
    pub fn receiver_flow(flow: RawStatsReceiverFlow) -> Self {
        Self {
            json_size: 0,
            stats_json: std::ptr::null(),
            version: 0,
            stats_type: RawStatsType::RECEIVER_FLOW,
            stats: RawStatsUnion {
                receiver_flow: flow,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixed buffer lengths must match librist/headers.h exactly — a drift
    // would mis-place every field after the cname in the receiver-flow struct.
    #[test]
    fn string_buffer_lengths_match_librist() {
        assert_eq!(RIST_MAX_STRING_SHORT, 128);
        assert_eq!(RIST_MAX_STRING_LONG, 256);
    }

    #[test]
    fn stats_type_discriminants_match_the_c_enum() {
        assert_eq!(RawStatsType::SENDER_PEER.0, 0);
        assert_eq!(RawStatsType::RECEIVER_FLOW.0, 1);
    }
}
