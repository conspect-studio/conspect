#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Offline decode tests for the librist C-ABI stats blob (ADR-0095 RIST-5).
//!
//! These feed a **fake** librist-shaped raw stats struct (the exact `repr(C)`
//! layout of librist 0.2.x `struct rist_stats`, with its sender-peer / receiver-
//! flow union) into the pure decoder and assert it lowers to the neutral
//! `multiview_telemetry::RistLinkSample`. No librist, no `dlopen`, no socket — the
//! decode is the load-bearing parse and it must be testable with no native dep.

use multiview_rist_sys::raw::{
    cchar_from_u8, RawStats, RawStatsReceiverFlow, RawStatsSenderPeer, RawStatsType,
    RIST_MAX_STRING_LONG, RIST_MAX_STRING_SHORT,
};
use multiview_rist_sys::{decode_stats, DecodeError};
use multiview_telemetry::rist::RistLinkRole;

fn cstr_short(s: &str) -> [std::os::raw::c_char; RIST_MAX_STRING_SHORT] {
    let mut buf = [cchar_from_u8(0); RIST_MAX_STRING_SHORT];
    for (slot, byte) in buf.iter_mut().zip(s.bytes()) {
        *slot = cchar_from_u8(byte);
    }
    buf
}

fn cstr_long(s: &str) -> [std::os::raw::c_char; RIST_MAX_STRING_LONG] {
    let mut buf = [cchar_from_u8(0); RIST_MAX_STRING_LONG];
    for (slot, byte) in buf.iter_mut().zip(s.bytes()) {
        *slot = cchar_from_u8(byte);
    }
    buf
}

#[test]
fn decodes_a_sender_peer_stats_blob() {
    let mut peer = RawStatsSenderPeer::zeroed();
    peer.cname = cstr_short("multiview-egress");
    peer.peer_id = 42;
    peer.bandwidth = 12_000_000;
    peer.retry_bandwidth = 24_000;
    peer.sent = 1_000_000;
    peer.received = 0;
    peer.retransmitted = 128;
    peer.quality = 99.5;
    peer.rtt = 42;

    let raw = RawStats::sender_peer(peer);
    let sample = decode_stats(&raw, "out-rist", 1_700).expect("sender stats decode");

    assert_eq!(sample.link_id, "out-rist");
    assert_eq!(sample.role, RistLinkRole::Sender);
    assert_eq!(sample.cname, "multiview-egress");
    assert_eq!(sample.rtt_ms, 42);
    assert_eq!(sample.bandwidth_bps, 12_000_000);
    assert_eq!(sample.retry_bandwidth_bps, 24_000);
    assert_eq!(sample.sent, 1_000_000);
    assert_eq!(sample.retransmitted, 128);
    assert_eq!(sample.peer_count, 1, "a sender peer is a single peer");
    assert!((sample.quality - 99.5).abs() < 1e-9);
    assert_eq!(sample.since, 1_700);
    // The sender-peer struct has no recovered/lost fields — they decode to 0.
    assert_eq!(sample.lost, 0);
    assert_eq!(sample.recovered, 0);
    assert_eq!(sample.received, 0);
}

#[test]
fn decodes_a_receiver_flow_stats_blob() {
    let mut flow = RawStatsReceiverFlow::zeroed();
    flow.peer_count = 2;
    flow.cname = cstr_long("peerA,peerB");
    flow.flow_id = 0x1234_5678;
    flow.bandwidth = 9_000_000;
    flow.retry_bandwidth = 100_000;
    flow.sent = 0;
    flow.received = 2_000_000;
    flow.missing = 300;
    flow.reordered = 12;
    flow.recovered = 290;
    flow.recovered_one_retry = 250;
    flow.lost = 10;
    flow.quality = 98.0;
    flow.rtt = 55;

    let raw = RawStats::receiver_flow(flow);
    let sample = decode_stats(&raw, "in-rist", 2_500).expect("receiver stats decode");

    assert_eq!(sample.link_id, "in-rist");
    assert_eq!(sample.role, RistLinkRole::Receiver);
    assert_eq!(sample.cname, "peerA,peerB");
    assert_eq!(sample.flow_id, 0x1234_5678);
    assert_eq!(sample.peer_count, 2);
    assert_eq!(sample.rtt_ms, 55);
    assert_eq!(sample.bandwidth_bps, 9_000_000);
    assert_eq!(sample.received, 2_000_000);
    assert_eq!(sample.recovered, 290);
    assert_eq!(sample.lost, 10);
    // librist's receiver-flow has no single "retransmitted" field on the receive
    // side; the recovered count is the ARQ work seen. retransmitted decodes to 0.
    assert_eq!(sample.retransmitted, 0);
    assert!((sample.quality - 98.0).abs() < 1e-9);
}

#[test]
fn rejects_an_unknown_stats_type() {
    // A stats_type the binding does not understand must surface a typed error,
    // never silently mis-decode the union (a forward-compat / corruption guard).
    let mut raw = RawStats::sender_peer(RawStatsSenderPeer::zeroed());
    raw.stats_type = RawStatsType(9999);
    let err = decode_stats(&raw, "x", 0).unwrap_err();
    assert!(matches!(err, DecodeError::UnknownStatsType(9999)));
}

#[test]
fn truncated_cname_without_a_nul_does_not_overrun() {
    // A cname that fills the whole fixed buffer with no NUL terminator must be
    // read up to the buffer bound, never past it (no UB / overrun).
    let mut peer = RawStatsSenderPeer::zeroed();
    peer.cname = [cchar_from_u8(b'A'); RIST_MAX_STRING_SHORT];
    let raw = RawStats::sender_peer(peer);
    let sample = decode_stats(&raw, "out", 0).expect("decode bounded cname");
    assert_eq!(sample.cname.len(), RIST_MAX_STRING_SHORT);
    assert!(sample.cname.bytes().all(|b| b == b'A'));
}
