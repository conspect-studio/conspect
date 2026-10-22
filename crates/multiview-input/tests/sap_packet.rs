//! RFC 2974 **SAP packet codec** tests — parse/encode round-trip, the golden
//! wire vectors, the `auth_len`-as-32-bit-WORDS regression (NOT VLC's byte
//! bug), the `C=1` capped zlib-inflate (with its decompression-bomb guard), and
//! the security rejections (encrypted, zero-hash, short buffer, wrong version).
//!
//! SAP = **Session Announcement Protocol** (RFC 2974), *not* subtitles. See
//! [ADR-0041] + `docs/research/sap-discovery.md` §3.
//!
//! [ADR-0041]: ../../docs/decisions/ADR-0041.md
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

use proptest::prelude::*;

use multiview_input::sap::packet::{MAX_SDP_PAYLOAD, SAP_VERSION, SDP_MIME_TYPE};
use multiview_input::sap::{SapError, SapMessageType, SapPacket};

/// A representative SDP body (opaque to the codec — begins `v=0`).
const SDP: &[u8] = b"v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=Multiview Test\r\nt=0 0\r\n";

/// `zlib([SDP])` produced by **real zlib** (Python `zlib.compress`, *not* the
/// decoder's own library) — a genuine interop vector for a `C=1` body whose
/// inflated form begins `v=0` (so the MIME type is omitted). Inflates to 56
/// bytes == [`SDP`], proving the parser inflates real gear's compressed
/// announcements (VLC/Dante), not just a miniz round-trip.
const ZLIB_OF_SDP: &[u8] = &[
    0x78, 0x9c, 0x2b, 0xb3, 0x35, 0xe0, 0xe5, 0xca, 0xb7, 0xd5, 0x55, 0x30, 0x04, 0x42, 0x4f, 0x3f,
    0x05, 0xcf, 0x00, 0x13, 0x05, 0x43, 0x4b, 0x23, 0x3d, 0x03, 0x3d, 0x23, 0x3d, 0x43, 0x5e, 0xae,
    0x62, 0x5b, 0xdf, 0xd2, 0x9c, 0x92, 0xcc, 0xb2, 0xcc, 0xd4, 0x72, 0x85, 0x90, 0xd4, 0xe2, 0x12,
    0x5e, 0xae, 0x12, 0x5b, 0x03, 0x05, 0xa0, 0x16, 0x00, 0x73, 0x3b, 0x0d, 0x9f,
];

/// `zlib("application/sdp\0" + [SDP])` (real zlib) — proves RFC 2974 §3: when
/// `C=1`, the **payload-type field is inside** the compressed region. Inflates
/// to 72 bytes ("application/sdp\0" + `SDP`), which then splits into
/// `Some("application/sdp")` + `SDP`.
const ZLIB_OF_MIME_SDP: &[u8] = &[
    0x78, 0x9c, 0x4b, 0x2c, 0x28, 0xc8, 0xc9, 0x4c, 0x4e, 0x2c, 0xc9, 0xcc, 0xcf, 0xd3, 0x2f, 0x4e,
    0x29, 0x60, 0x28, 0xb3, 0x35, 0xe0, 0xe5, 0xca, 0xb7, 0xd5, 0x55, 0x30, 0x04, 0x42, 0x4f, 0x3f,
    0x05, 0xcf, 0x00, 0x13, 0x05, 0x43, 0x4b, 0x23, 0x3d, 0x03, 0x3d, 0x23, 0x3d, 0x43, 0x5e, 0xae,
    0x62, 0x5b, 0xdf, 0xd2, 0x9c, 0x92, 0xcc, 0xb2, 0xcc, 0xd4, 0x72, 0x85, 0x90, 0xd4, 0xe2, 0x12,
    0x5e, 0xae, 0x12, 0x5b, 0x03, 0x05, 0xa0, 0x16, 0x00, 0xfc, 0x79, 0x13, 0xa9,
];

/// `zlib(200000 × 0x00)` (real zlib) — a **decompression bomb**: 217 compressed
/// bytes that inflate to 200000, far past [`MAX_SDP_PAYLOAD`] (64 KiB). It must
/// be rejected AT the inflate cap (`miniz_oxide` stops), never allocated in full.
const ZLIB_BOMB: &[u8] = &[
    0x78, 0x9c, 0xed, 0xc1, 0x31, 0x01, 0x00, 0x00, 0x00, 0xc2, 0xa0, 0xf5, 0x4f, 0x6d, 0x06, 0x7f,
    0xa0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x80, 0xd7, 0x00, 0x0d, 0x6d, 0x00, 0x01,
];

/// Build a common IPv4 SAP header prefix (flags, auth-len, hash, 4-byte origin).
fn v4_header(flags: u8, auth_len: u8, hash: u16, origin: Ipv4Addr) -> Vec<u8> {
    let mut b = vec![flags, auth_len];
    b.extend_from_slice(&hash.to_be_bytes());
    b.extend_from_slice(&origin.octets());
    b
}

#[test]
fn parses_ipv4_announcement_with_omitted_payload_type() {
    let mut bytes = v4_header(0x20, 0x00, 0x1234, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).expect("valid announcement parses");
    assert_eq!(pkt.message_type, SapMessageType::Announcement);
    assert_eq!(pkt.msg_id_hash.get(), 0x1234);
    assert_eq!(pkt.origin, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
    assert_eq!(
        pkt.payload_type, None,
        "an SDP body beginning v=0 omits the MIME type"
    );
    assert_eq!(pkt.payload.as_slice(), SDP);
}

#[test]
fn encode_is_the_inverse_of_parse_for_the_golden_vector() {
    let mut bytes = v4_header(0x20, 0x00, 0x1234, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(
        pkt.encode(),
        bytes,
        "encode(parse(x)) == x for the golden vector"
    );
}

#[test]
fn parses_explicit_application_sdp_payload_type() {
    let mut bytes = v4_header(0x20, 0x00, 0x0001, Ipv4Addr::new(203, 0, 113, 5));
    bytes.extend_from_slice(SDP_MIME_TYPE.as_bytes());
    bytes.push(0x00); // NUL terminator
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(pkt.payload_type.as_deref(), Some("application/sdp"));
    assert_eq!(pkt.payload.as_slice(), SDP);
    assert_eq!(
        pkt.encode(),
        bytes,
        "explicit MIME round-trips byte-identically"
    );
}

#[test]
fn parses_ipv6_origin_when_address_bit_set() {
    let origin = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
    let mut bytes = vec![0x30u8, 0x00]; // V=1 (top 3 bits), A=1 (IPv6 origin)
    bytes.extend_from_slice(&0xBEEFu16.to_be_bytes());
    bytes.extend_from_slice(&origin.octets());
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(pkt.origin, IpAddr::V6(origin));
    assert_eq!(pkt.msg_id_hash.get(), 0xBEEF);
    assert_eq!(pkt.encode(), bytes);
}

#[test]
fn parses_deletion_message_type() {
    let mut bytes = v4_header(0x24, 0x00, 0x00AA, Ipv4Addr::new(192, 0, 2, 9)); // T=1 (0x04)
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(pkt.message_type, SapMessageType::Deletion);
}

#[test]
fn auth_len_is_skipped_as_32bit_words_not_bytes() {
    // auth_len = 1 word = 4 bytes of auth data. A compliant parser skips 4 bytes;
    // VLC's `buf += auth_len` bug skips 1. Placing exactly 4 auth bytes then the
    // SDP proves the skip is words*4 (the SDP still lands and begins `v=0`).
    let mut bytes = v4_header(0x20, 0x01, 0x1000, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // 1 word (4 bytes) of auth
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).expect("auth is skipped, the SDP parses");
    assert_eq!(
        pkt.payload_type, None,
        "4 auth bytes skipped => the body begins v=0 (type omitted)"
    );
    assert_eq!(
        pkt.payload.as_slice(),
        SDP,
        "auth_len counts 32-bit WORDS (skip words*4), never VLC's byte count"
    );
}

#[test]
fn rejects_encrypted_packets() {
    let mut bytes = v4_header(0x22, 0x00, 0x0001, Ipv4Addr::new(192, 0, 2, 1)); // E=1 (0x02)
    bytes.extend_from_slice(SDP);
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::Encrypted));
}

#[test]
fn parses_compressed_c1_body_inflated_under_cap() {
    // C=1 (0x01): the post-auth body is a real zlib stream. The parser inflates
    // it under a hard cap and yields the decompressed SDP (begins v=0 => no MIME).
    let mut bytes = v4_header(0x21, 0x00, 0x0001, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(ZLIB_OF_SDP);
    let pkt = SapPacket::parse(&bytes).expect("a valid C=1 body inflates and parses");
    assert_eq!(pkt.msg_id_hash.get(), 0x0001);
    assert_eq!(
        pkt.payload_type, None,
        "the inflated body begins v=0 => MIME omitted"
    );
    assert_eq!(
        pkt.payload.as_slice(),
        SDP,
        "the inflated payload is the original SDP"
    );
}

#[test]
fn compressed_c1_payload_type_is_inside_the_compressed_region() {
    // RFC 2974 §3: with C=1 BOTH the payload-type field and the payload are
    // compressed together. Inflating must recover the MIME type, then split it.
    let mut bytes = v4_header(0x21, 0x00, 0x0002, Ipv4Addr::new(203, 0, 113, 5));
    bytes.extend_from_slice(ZLIB_OF_MIME_SDP);
    let pkt = SapPacket::parse(&bytes).expect("C=1 with an inner MIME type parses");
    assert_eq!(pkt.payload_type.as_deref(), Some("application/sdp"));
    assert_eq!(pkt.payload.as_slice(), SDP);
}

#[test]
fn rejects_compression_bomb_over_the_cap() {
    // A tiny datagram that inflates past MAX_SDP_PAYLOAD is a decompression bomb:
    // rejected AT the inflate cap (miniz stops), never allocated in full.
    let mut bytes = v4_header(0x21, 0x00, 0x0003, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(ZLIB_BOMB);
    assert_eq!(
        SapPacket::parse(&bytes),
        Err(SapError::DecompressedTooLarge {
            max: MAX_SDP_PAYLOAD
        }),
        "a >64KiB decompression bomb is rejected at the cap"
    );
}

#[test]
fn rejects_corrupt_compressed_stream() {
    // C=1 whose body is not a valid zlib stream: dropped (SAP is unauthenticated,
    // a malformed compressed announcement is simply discarded), never a panic.
    let mut bytes = v4_header(0x21, 0x00, 0x0004, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05]); // not a zlib stream
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::DecompressFailed));
}

/// Frame a `C=1` (compressed) announcement whose body is `zlib(raw)`, using
/// **real** zlib (`miniz_oxide` deflate — the deflate side of the same library
/// the codec inflates with). Choosing `raw` chooses the *decompressed* size
/// exactly, so a test can craft a body that lands right at, one byte past, or
/// far past the [`MAX_SDP_PAYLOAD`] inflate cap.
fn c1_datagram(hash: u16, raw: &[u8]) -> Vec<u8> {
    let mut bytes = v4_header(0x21, 0x00, hash, Ipv4Addr::new(192, 0, 2, 1)); // C=1
    bytes.extend_from_slice(&miniz_oxide::deflate::compress_to_vec_zlib(raw, 6));
    bytes
}

#[test]
fn rejects_a_bomb_whose_decompressed_size_dwarfs_the_cap() {
    // A *verified*, not merely asserted, bomb-safety proof (rule 23/25). This is a
    // valid zlib stream whose decompressed form is 128x the cap (= 8 MiB) — defined
    // as a multiple of the cap so it can never silently drift below it.
    // `decompress_to_vec_zlib_with_limit` bounds the output buffer AT the cap and
    // returns `HasMoreOutput` the instant inflate would write past it, so a
    // `DecompressedTooLarge` here is a direct witness that the parser halted at
    // 64 KiB and NEVER materialized the 8 MiB: peak allocation is bounded by the
    // cap, by construction, on an adversarial input that dwarfs it. Complements
    // `rejects_compression_bomb_over_the_cap` (a real *external*-zlib 195 KiB bomb).
    const NOMINAL: usize = 128 * MAX_SDP_PAYLOAD; // 8 MiB
    let datagram = c1_datagram(0x0005, &vec![0u8; NOMINAL]);
    // The datagram itself is tiny: a few KiB of zlib carries the 8 MiB bomb — the
    // definition of a decompression bomb (a small input, an unbounded expansion).
    assert!(
        datagram.len() < MAX_SDP_PAYLOAD,
        "the compressed bomb datagram ({} bytes) is far smaller than its 8 MiB payload",
        datagram.len()
    );
    assert_eq!(
        SapPacket::parse(&datagram),
        Err(SapError::DecompressedTooLarge {
            max: MAX_SDP_PAYLOAD
        }),
        "an 8 MiB-nominal C=1 bomb is rejected AT the cap, never inflated in full"
    );
}

#[test]
fn inflates_a_body_exactly_at_the_cap_and_rejects_one_byte_over() {
    // The cap is an EXACT threshold, not approximate: a body inflating to exactly
    // MAX_SDP_PAYLOAD is accepted (legitimate near-cap Dante/VLC announcements must
    // still parse — the guard must not be over-tight), while MAX_SDP_PAYLOAD + 1 is
    // rejected. Both bodies begin `v=0`, so the whole inflated body is the
    // (typeless) payload.
    let mut at_cap = b"v=0\r\n".to_vec();
    at_cap.resize(MAX_SDP_PAYLOAD, b'.');
    let pkt = SapPacket::parse(&c1_datagram(0x0006, &at_cap))
        .expect("a body inflating to exactly the cap is accepted");
    assert_eq!(pkt.payload_type, None);
    assert_eq!(pkt.payload.len(), MAX_SDP_PAYLOAD);
    assert_eq!(pkt.payload.as_slice(), at_cap.as_slice());

    let mut over_cap = at_cap;
    over_cap.push(b'.'); // one byte past the cap
    assert_eq!(
        SapPacket::parse(&c1_datagram(0x0007, &over_cap)),
        Err(SapError::DecompressedTooLarge {
            max: MAX_SDP_PAYLOAD
        }),
        "one byte past the cap is rejected during inflate"
    );
}

#[test]
fn rejects_zero_message_id_hash() {
    let mut bytes = v4_header(0x20, 0x00, 0x0000, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(SDP);
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::ZeroHash));
}

#[test]
fn rejects_wrong_version() {
    let mut bytes = v4_header(0x40, 0x00, 0x0001, Ipv4Addr::new(192, 0, 2, 1)); // V=2 (2<<5)
    bytes.extend_from_slice(SDP);
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::BadVersion(2)));
}

#[test]
fn rejects_short_buffers_without_panicking() {
    let cases: [&[u8]; 6] = [
        &[],
        &[0x20],
        &[0x20, 0x00],
        &[0x20, 0x00, 0x12],
        &[0x20, 0x00, 0x12, 0x34],
        &[0x20, 0x00, 0x12, 0x34, 0xC0, 0x00, 0x02],
    ];
    for truncated in cases {
        assert!(
            SapPacket::parse(truncated).is_err(),
            "short buffer {truncated:?} must error, never panic"
        );
    }
}

#[test]
fn encode_never_emits_zero_hash_encryption_or_compression() {
    let pkt = SapPacket {
        message_type: SapMessageType::Announcement,
        msg_id_hash: NonZeroU16::new(0x4d56).unwrap(),
        origin: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        payload_type: Some(SDP_MIME_TYPE.to_owned()),
        payload: SDP.to_vec(),
    };
    let bytes = pkt.encode();
    let flags = bytes[0];
    assert_eq!(
        flags >> 5,
        SAP_VERSION,
        "version field is the top 3 bits = 1"
    );
    assert_eq!(flags & 0x02, 0, "E (encryption) is never set on emit");
    assert_eq!(flags & 0x01, 0, "C (compression) is never set on emit");
    assert_eq!(bytes[1], 0, "our announcer emits auth_len = 0");
    assert_ne!(
        u16::from_be_bytes([bytes[2], bytes[3]]),
        0,
        "the message-id hash is never emitted as 0"
    );
}

/// Arbitrary origin: an IPv4 (A=0) or IPv6 (A=1) address.
fn arb_origin() -> impl Strategy<Value = IpAddr> {
    prop_oneof![
        any::<[u8; 4]>().prop_map(|o| IpAddr::V4(Ipv4Addr::from(o))),
        any::<[u8; 16]>().prop_map(|o| IpAddr::V6(Ipv6Addr::from(o))),
    ]
}

/// Arbitrary well-formed packet. The payload-type variants mirror the two real
/// forms: an omitted type (body begins `v=0`) or an explicit MIME type (no NUL,
/// not starting `v=0`) followed by an opaque payload.
fn arb_packet() -> impl Strategy<Value = SapPacket> {
    let msg_type = prop_oneof![
        Just(SapMessageType::Announcement),
        Just(SapMessageType::Deletion),
    ];
    let hash = (1u16..=u16::MAX).prop_map(|h| NonZeroU16::new(h).unwrap());
    let payload_variant = prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..256).prop_map(|tail| {
            let mut p = b"v=0".to_vec();
            p.extend_from_slice(&tail);
            (None, p)
        }),
        (
            prop_oneof![
                Just("application/sdp"),
                Just("text/plain"),
                Just("application/x-multiview"),
            ],
            proptest::collection::vec(any::<u8>(), 0..256),
        )
            .prop_map(|(m, p)| (Some(m.to_owned()), p)),
    ];
    (msg_type, hash, arb_origin(), payload_variant).prop_map(
        |(message_type, msg_id_hash, origin, (payload_type, payload))| SapPacket {
            message_type,
            msg_id_hash,
            origin,
            payload_type,
            payload,
        },
    )
}

proptest! {
    /// Round-trip identity: `parse(encode(pkt)) == pkt` over all valid inputs.
    #[test]
    fn round_trip_encode_then_parse_is_identity(pkt in arb_packet()) {
        let encoded = pkt.encode();
        let parsed = SapPacket::parse(&encoded).expect("a well-formed packet parses");
        prop_assert_eq!(parsed, pkt);
    }
}
