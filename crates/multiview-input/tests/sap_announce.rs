//! SAP **announce schedule + packet builders** tests (RFC 2974 §3/§5/§6;
//! ADR-0041 §5, brief §3).
//!
//! The schedule is pure: a ≥ 30 s base cadence (Dante's interop default) with
//! **±1/3 jitter** (`offset = rand(interval·2/3) − interval/3`) so many announcers
//! de-synchronise. The builders produce a `T=0` announcement carrying the
//! `application/sdp` payload-type and a courtesy `T=1` deletion, both keyed by a
//! **stable, non-zero** per-output hash.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use multiview_input::sap::announce::{
    announcement, deletion, stable_hash, AnnounceSchedule, MIN_ANNOUNCE_INTERVAL,
};
use multiview_input::sap::packet::{SapMessageType, SapPacket, SDP_MIME_TYPE};
use proptest::prelude::*;

fn origin() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn base_interval_has_a_thirty_second_floor() {
    assert_eq!(MIN_ANNOUNCE_INTERVAL, secs(30));
    assert_eq!(
        AnnounceSchedule::new(secs(5)).base_interval(),
        secs(30),
        "a sub-floor request is clamped up to the 30 s interop floor"
    );
    assert_eq!(
        AnnounceSchedule::new(secs(90)).base_interval(),
        secs(90),
        "a larger cadence is honoured"
    );
}

#[test]
fn jitter_endpoints_are_two_thirds_and_just_under_four_thirds() {
    let sched = AnnounceSchedule::new(secs(30));
    // sample 0 → the low endpoint 2/3·base = 20 s.
    assert_eq!(sched.next_delay(0), secs(20));
    // sample MAX → approaches, but never reaches, 4/3·base = 40 s.
    let hi = sched.next_delay(u64::MAX);
    assert!(hi < secs(40), "jitter never reaches the open upper bound");
    assert!(
        hi >= secs(39),
        "sample MAX lands just under 40 s, got {hi:?}"
    );
    // mid sample → about the base cadence.
    let mid = sched.next_delay(u64::MAX / 2);
    assert!(
        mid >= secs(29) && mid <= secs(31),
        "the mid sample is about the base cadence, got {mid:?}"
    );
}

#[test]
fn announcement_is_a_t0_packet_carrying_the_sdp_mime_type() {
    let sdp = b"v=0\r\no=- 1 1 IN IP4 192.0.2.10\r\ns=multiview\r\n".to_vec();
    let h = stable_hash(&sdp);
    let pkt = announcement(h, origin(), sdp.clone());
    assert_eq!(pkt.message_type, SapMessageType::Announcement);
    assert_eq!(pkt.msg_id_hash, h);
    assert_eq!(pkt.origin, origin());
    assert_eq!(pkt.payload_type.as_deref(), Some(SDP_MIME_TYPE));
    assert_eq!(pkt.payload, sdp);
    // Round-trips through the wire codec (application/sdp\0 prefix + body).
    let round = SapPacket::parse(&pkt.encode()).unwrap();
    assert_eq!(round, pkt);
}

#[test]
fn deletion_is_a_t1_packet_for_the_same_session() {
    let sdp = b"v=0\r\no=- 7 7 IN IP4 192.0.2.10\r\n".to_vec();
    let h = stable_hash(&sdp);
    let pkt = deletion(h, origin(), sdp.clone());
    assert_eq!(pkt.message_type, SapMessageType::Deletion);
    assert_eq!(
        pkt.msg_id_hash, h,
        "the delete carries the announcement's hash"
    );
    assert_eq!(pkt.origin, origin());
    let round = SapPacket::parse(&pkt.encode()).unwrap();
    assert_eq!(round.message_type, SapMessageType::Deletion);
    assert_eq!(round.msg_id_hash, h);
}

#[test]
fn stable_hash_is_deterministic_and_content_sensitive() {
    let a = b"v=0\r\no=- 1 1 IN IP4 239.255.0.1\r\ns=alpha\r\n";
    let b = b"v=0\r\no=- 2 2 IN IP4 239.255.0.2\r\ns=beta\r\n";
    assert_eq!(stable_hash(a), stable_hash(a), "same content → same hash");
    assert_ne!(
        stable_hash(a),
        stable_hash(b),
        "different content → different hash (modification is detectable)"
    );
    // The type guarantees non-zero (0 is the reserved hash) — no assertion needed
    // beyond construction succeeding for empty input.
    let _ = stable_hash(b"");
}

#[test]
fn stable_hash_matches_pinned_fnv1a_golden_vectors() {
    // P2-F4: `stable_hash` must be pinned to a DEFINED, cross-version-stable
    // algorithm (FNV-1a), never `DefaultHasher` — whose output Rust explicitly
    // does not guarantee across toolchain versions. A drift would change the
    // message-id hash for UNCHANGED SDP across a restart, so a receiver could not
    // correlate an announce/delete pair (RFC 2974 §8) and would carry a duplicate
    // session until timeout. These golden vectors are the FNV-1a-of-SDP contract:
    // they FAIL against `DefaultHasher` and pin the algorithm against any future
    // change. Computed from FNV-1a/64 (offset 0xcbf2_9ce4_8422_2325,
    // prime 0x0000_0100_0000_01b3), XOR-folded to 16 bits, forced non-zero —
    // independent of the toolchain.
    assert_eq!(stable_hash(b"").get(), 0xF011, "empty SDP");
    assert_eq!(stable_hash(b"v=0\r\n").get(), 0xB0F8, "minimal SDP body");
    let sdp = b"v=0\r\no=- 1 1 IN IP6 2001:db8::1\r\ns=mv\r\nt=0 0\r\nm=audio 5004 RTP/AVP 96\r\n";
    assert_eq!(stable_hash(sdp).get(), 0xE2FE, "realistic IPv6 SDP");
    // A one-character change (session-id `1` -> `2`) yields a DIFFERENT pinned
    // hash, so a modification is still detectable under the stable algorithm.
    let modified =
        b"v=0\r\no=- 1 2 IN IP6 2001:db8::1\r\ns=mv\r\nt=0 0\r\nm=audio 5004 RTP/AVP 96\r\n";
    assert_eq!(stable_hash(modified).get(), 0x581B, "one-byte-diff SDP");
    assert_ne!(
        stable_hash(sdp),
        stable_hash(modified),
        "a modified SDP is detectable (different stable hash)"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For ANY base cadence and ANY jitter sample the next delay stays within the
    /// ±1/3 window `[2/3·base, 4/3·base]` (brief §3): no announcer ever undershoots
    /// or overshoots the jitter bound.
    #[test]
    fn jitter_stays_within_the_plus_minus_one_third_window(
        base_s in 30u64..300,
        sample in any::<u64>(),
    ) {
        let base = secs(base_s);
        let sched = AnnounceSchedule::new(base);
        let delay = sched.next_delay(sample);
        prop_assert!(delay >= base * 2 / 3, "below 2/3·base: {delay:?}");
        prop_assert!(delay <= base * 4 / 3, "above 4/3·base: {delay:?}");
    }
}
