//! RFC 9443 first-byte demultiplexing for the shared UDP socket (design note
//! `docs/architecture/network-front-door.md` section 3).
//!
//! Two steps, both pure and allocation-free:
//!
//! 1. [`first_byte_class`] maps the first byte to exactly one [`ByteClass`]
//!    (the table in section 3; unit-tested for every value `0..=255`).
//! 2. [`classify`] adds the cheap structural checks each consumer would make
//!    anyway, so a datagram that cannot be what its first byte claims is dropped
//!    here and never handed to the wrong (or any) consumer.
//!
//! QUIC bit greasing (RFC 9287) is off on the shared endpoint
//! ([`crate::quic::endpoint_config`]), so every compliant v1 long-header packet a
//! client sends is `0b11xxxxxx` (192-255) and every short-header packet is
//! `0b01xxxxxx` (64-127). A QUIC Version Negotiation packet (top bit only, RFC 8999
//! section 5.3) whose first byte lands in 128-191 is classified RTP by the table
//! and then rejected by the RTP check only if it is malformed RTP; a server never
//! needs to receive VN, and str0m drops it as belonging to no session. A VN in
//! 192-255 is dropped here (version 0).

use std::net::SocketAddr;

/// QUIC versions the shared endpoint accepts (set explicitly on the noq
/// endpoint so this list and the endpoint can never disagree): QUIC v1 and the
/// drafts noq 1.3.5 still speaks. QUIC v2 (RFC 9369, `0x6b3343cf`) is not
/// supported by noq 1.3.5: a v2 Initial is a Version Negotiation trigger.
pub const SUPPORTED_QUIC_VERSIONS: &[u32] = &[
    0x0000_0001,
    0xff00_001d,
    0xff00_001e,
    0xff00_001f,
    0xff00_0020,
    0xff00_0021,
    0xff00_0022,
];

/// The QUIC v2 version number (RFC 9369), named for tests and logs.
pub const QUIC_V2: u32 = 0x6b33_43cf;

/// RFC 9000 section 14.1: a client pads every datagram carrying an Initial to at
/// least 1200 bytes. Smaller Initials (and smaller unsupported-version long
/// headers, which would draw a Version Negotiation reply) are dropped here, so a
/// Version Negotiation reply (about 60 bytes) is never larger than its trigger.
pub const MIN_INITIAL_DATAGRAM: usize = 1200;

/// The shortest QUIC short-header packet our server can be sent: 1 flags byte,
/// an 8-byte connection ID (noq's default), at least 1 byte of packet number and
/// a 16-byte AEAD tag would be 26; RFC 9000 section 10.3 makes 21 the smallest
/// stateless reset, so 21 is the conservative floor.
pub const MIN_SHORT_HEADER: usize = 21;

/// The STUN magic cookie (RFC 8489 section 5).
pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

/// What the first byte alone says (RFC 9443 section 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ByteClass {
    /// 0-3: STUN (and TURN control).
    Stun,
    /// 4-15 (RFC 9443: MUST drop) and 16-19 (ZRTP, never negotiated).
    Drop,
    /// 20-63: DTLS.
    Dtls,
    /// 64-79: TURN `ChannelData` when the source is a TURN server we allocated
    /// on, otherwise a QUIC short header.
    ChannelDataOrQuicShort,
    /// 80-127: QUIC short header.
    QuicShort,
    /// 128-191: RTP/RTCP.
    Rtp,
    /// 192-255: QUIC long header.
    QuicLong,
}

/// The RFC 9443 first-byte table. Total: every byte maps to exactly one class.
#[must_use]
pub const fn first_byte_class(byte: u8) -> ByteClass {
    match byte {
        0..=3 => ByteClass::Stun,
        4..=19 => ByteClass::Drop,
        20..=63 => ByteClass::Dtls,
        64..=79 => ByteClass::ChannelDataOrQuicShort,
        80..=127 => ByteClass::QuicShort,
        128..=191 => ByteClass::Rtp,
        192..=255 => ByteClass::QuicLong,
    }
}

/// Which consumer a datagram goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// The QUIC endpoint (`MoQ` / WebTransport).
    Quic,
    /// The WebRTC endpoint (str0m: STUN, DTLS, SRTP/SRTCP, TURN).
    Rtc,
}

/// The QUIC packet shapes the dispatcher treats differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuicKind {
    /// A supported-version Initial (padded to >= 1200 bytes): the only packet
    /// that makes noq create connection state for an unvalidated address, so it
    /// is charged to a global handshake budget as well as the new-flow limits.
    Initial,
    /// A long header with a supported version that is not an Initial (0-RTT,
    /// Handshake).
    Long,
    /// A short header (1-RTT data, or a stateless-reset trigger when the
    /// connection ID is unknown).
    Short,
    /// A long header with an unsupported version, at least
    /// [`MIN_INITIAL_DATAGRAM`] bytes: noq answers it with Version Negotiation,
    /// outside the 3x anti-amplification limit, so it is charged to a separate
    /// global budget as well as the new-flow limits.
    VersionNegotiationTrigger,
}

/// Why a datagram was dropped by the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// Zero-length datagram.
    Empty,
    /// First byte 4-19 (RFC 9443 section 3).
    Reserved,
    /// Claims STUN but is not a well-formed STUN header.
    BadStun,
    /// Claims DTLS but is shorter than a record header.
    BadDtls,
    /// Claims RTP/RTCP but is shorter than an RTCP header.
    BadRtp,
    /// TURN `ChannelData` shorter than its 4-byte header.
    BadChannelData,
    /// Claims a QUIC short header but is shorter than any packet our server
    /// can be sent.
    ShortQuicShort,
    /// Claims a QUIC long header but is truncated or has an illegal CID length.
    BadQuicLong,
    /// A QUIC Version Negotiation packet (version 0). A server never needs one.
    VersionNegotiation,
    /// A QUIC Retry packet. Only a client receives Retry.
    Retry,
    /// A supported-version Initial in a datagram under 1200 bytes.
    ShortInitial,
    /// An unsupported-version long header under 1200 bytes (would draw a VN
    /// reply larger than the trigger).
    ShortVersionNegotiationTrigger,
}

/// The classifier's verdict for one datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Deliver to the WebRTC lane.
    Rtc,
    /// Deliver to the QUIC lane.
    Quic(QuicKind),
    /// Drop and count.
    Drop(DropReason),
}

impl Verdict {
    /// The lane this verdict delivers to, if any.
    #[must_use]
    pub const fn lane(self) -> Option<Lane> {
        match self {
            Self::Rtc => Some(Lane::Rtc),
            Self::Quic(_) => Some(Lane::Quic),
            Self::Drop(_) => None,
        }
    }
}

/// Classify one datagram. `from_turn_server` says whether the source is a TURN
/// server the WebRTC endpoint allocated on (only then is 64-79 `ChannelData`).
#[must_use]
pub fn classify(datagram: &[u8], from_turn_server: bool) -> Verdict {
    let Some(&first) = datagram.first() else {
        return Verdict::Drop(DropReason::Empty);
    };
    match first_byte_class(first) {
        ByteClass::Stun => {
            if is_well_formed_stun(datagram) {
                Verdict::Rtc
            } else {
                Verdict::Drop(DropReason::BadStun)
            }
        }
        ByteClass::Drop => Verdict::Drop(DropReason::Reserved),
        ByteClass::Dtls => {
            if is_plausible_dtls(datagram) {
                Verdict::Rtc
            } else {
                Verdict::Drop(DropReason::BadDtls)
            }
        }
        ByteClass::ChannelDataOrQuicShort if from_turn_server => {
            // RFC 8656 section 12.4: 4-byte header (channel number, length).
            if datagram.len() >= 4 {
                Verdict::Rtc
            } else {
                Verdict::Drop(DropReason::BadChannelData)
            }
        }
        ByteClass::ChannelDataOrQuicShort | ByteClass::QuicShort => {
            if datagram.len() >= MIN_SHORT_HEADER {
                Verdict::Quic(QuicKind::Short)
            } else {
                Verdict::Drop(DropReason::ShortQuicShort)
            }
        }
        ByteClass::Rtp => {
            // RTCP's fixed header is 8 bytes, RTP's 12.
            if datagram.len() >= 8 {
                Verdict::Rtc
            } else {
                Verdict::Drop(DropReason::BadRtp)
            }
        }
        ByteClass::QuicLong => classify_long_header(first, datagram),
    }
}

/// The version field of a QUIC long header (bytes 1-4), if present.
#[must_use]
pub fn long_header_version(datagram: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = datagram.get(1..5)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn classify_long_header(first: u8, datagram: &[u8]) -> Verdict {
    let Some(version) = long_header_version(datagram) else {
        return Verdict::Drop(DropReason::BadQuicLong);
    };
    if version == 0 {
        return Verdict::Drop(DropReason::VersionNegotiation);
    }
    // RFC 9000 section 17.2: DCID length (byte 5) <= 20, then the DCID, then the
    // SCID length and the SCID. Checked before the version branch: noq answers
    // an unsupported version only for a header it can decode (review PR 315),
    // so an undecodable one must not spend the Version Negotiation budget.
    // (RFC 8999 allows longer CIDs for unknown versions; noq cannot parse them,
    // so it would send no reply anyway.)
    let Some(&dcid_len) = datagram.get(5) else {
        return Verdict::Drop(DropReason::BadQuicLong);
    };
    if dcid_len > 20 {
        return Verdict::Drop(DropReason::BadQuicLong);
    }
    let scid_len_at = 6 + usize::from(dcid_len);
    let Some(&scid_len) = datagram.get(scid_len_at) else {
        return Verdict::Drop(DropReason::BadQuicLong);
    };
    if scid_len > 20 || datagram.len() < scid_len_at + 1 + usize::from(scid_len) {
        return Verdict::Drop(DropReason::BadQuicLong);
    }
    if !SUPPORTED_QUIC_VERSIONS.contains(&version) {
        return if datagram.len() >= MIN_INITIAL_DATAGRAM {
            Verdict::Quic(QuicKind::VersionNegotiationTrigger)
        } else {
            Verdict::Drop(DropReason::ShortVersionNegotiationTrigger)
        };
    }
    // Long packet type, bits 4-5 (v1 and the drafts): 0 Initial, 1 0-RTT,
    // 2 Handshake, 3 Retry.
    match (first & 0x30) >> 4 {
        0 if datagram.len() < MIN_INITIAL_DATAGRAM => Verdict::Drop(DropReason::ShortInitial),
        0 => Verdict::Quic(QuicKind::Initial),
        3 => Verdict::Drop(DropReason::Retry),
        _ => Verdict::Quic(QuicKind::Long),
    }
}

/// A STUN header that str0m (and the TURN client) would parse: at least 20
/// bytes, the magic cookie, a message length that is a multiple of 4 and covers
/// exactly the rest of the datagram (RFC 8489 sections 5 and 6).
#[must_use]
pub fn is_well_formed_stun(datagram: &[u8]) -> bool {
    if datagram.len() < 20 {
        return false;
    }
    if datagram.get(4..8) != Some(&STUN_MAGIC_COOKIE[..]) {
        return false;
    }
    let Some(&[hi, lo]) = datagram.get(2..4) else {
        return false;
    };
    let declared = usize::from(u16::from_be_bytes([hi, lo]));
    declared % 4 == 0 && declared + 20 == datagram.len()
}

/// Whether a datagram the classifier delivers to the WebRTC lane is a STUN
/// message (first byte 0-3 and a well-formed header): the dispatcher charges
/// STUN from an unvalidated source to the STUN budgets ([`crate::Budget::Stun`]).
#[must_use]
pub fn is_stun(datagram: &[u8]) -> bool {
    matches!(datagram.first(), Some(0..=3)) && is_well_formed_stun(datagram)
}

/// A QUIC Version Negotiation shape in the RTP range (issue #325): first byte
/// 128-191 (only the top bit is fixed for VN, RFC 8999 section 5.3) and a zero
/// version field (bytes 1-4). As RTP that would be payload type 0, sequence 0
/// and timestamp high bytes 0, which a real stream sends at most once; the
/// dispatcher drops it only from a source that is **not** established
/// (unvalidated peers send no RTP before ICE and DTLS), so an established
/// peer's media is never touched.
#[must_use]
pub fn is_vn_shaped_rtp(datagram: &[u8]) -> bool {
    matches!(datagram.first(), Some(128..=191)) && datagram.get(1..5) == Some(&[0u8; 4][..])
}

/// A DTLS record long enough to carry a header: 13 bytes for the DTLS 1.2
/// record layer (content types 20-25), 5 for a DTLS 1.3 unified header
/// (`0b001xxxxx`, 32-63).
#[must_use]
pub fn is_plausible_dtls(datagram: &[u8]) -> bool {
    match datagram.first() {
        Some(20..=31) => datagram.len() >= 13,
        Some(32..=63) => datagram.len() >= 5,
        _ => false,
    }
}

/// The per-source key for new-flow limits: an IPv4 address (an IPv4-mapped IPv6
/// address counts as IPv4), or an IPv6 /64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKey {
    /// One IPv4 address.
    V4([u8; 4]),
    /// One IPv6 /64.
    V6([u8; 8]),
}

impl SourceKey {
    /// The key for a datagram source.
    #[must_use]
    pub fn of(addr: SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(v4) => Self::V4(v4.ip().octets()),
            SocketAddr::V6(v6) => {
                if let Some(v4) = v6.ip().to_ipv4_mapped() {
                    Self::V4(v4.octets())
                } else {
                    let o = v6.ip().octets();
                    Self::V6([o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]])
                }
            }
        }
    }
}

/// The canonical form of a peer address: an IPv4-mapped IPv6 address becomes
/// IPv4. Used for the established-flow set so a v4 peer is one key however the
/// dual-stack socket reported it.
#[must_use]
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::as_conversions)]
mod tests {
    use super::*;

    /// The design table, written out independently of `first_byte_class`.
    #[allow(
        clippy::if_same_then_else,
        reason = "two rows of the design table, kept separate"
    )]
    fn expected(byte: u8) -> ByteClass {
        if byte <= 3 {
            ByteClass::Stun
        } else if byte <= 15 {
            ByteClass::Drop // RFC 9443: MUST drop
        } else if byte <= 19 {
            ByteClass::Drop // ZRTP, never negotiated
        } else if byte <= 63 {
            ByteClass::Dtls
        } else if byte <= 79 {
            ByteClass::ChannelDataOrQuicShort
        } else if byte <= 127 {
            ByteClass::QuicShort
        } else if byte <= 191 {
            ByteClass::Rtp
        } else {
            ByteClass::QuicLong
        }
    }

    #[test]
    fn every_first_byte_has_exactly_one_class() {
        let mut counts = std::collections::HashMap::new();
        for byte in 0..=u8::MAX {
            let class = first_byte_class(byte);
            assert_eq!(class, expected(byte), "byte {byte}");
            *counts.entry(class).or_insert(0u32) += 1;
        }
        assert_eq!(counts[&ByteClass::Stun], 4);
        assert_eq!(counts[&ByteClass::Drop], 16);
        assert_eq!(counts[&ByteClass::Dtls], 44);
        assert_eq!(counts[&ByteClass::ChannelDataOrQuicShort], 16);
        assert_eq!(counts[&ByteClass::QuicShort], 48);
        assert_eq!(counts[&ByteClass::Rtp], 64);
        assert_eq!(counts[&ByteClass::QuicLong], 64);
        assert_eq!(counts.values().sum::<u32>(), 256);
    }

    #[test]
    fn channel_data_range_depends_only_on_the_turn_server_flag() {
        for byte in 64..=79u8 {
            let mut d = vec![0u8; 64];
            d[0] = byte;
            assert_eq!(classify(&d, true), Verdict::Rtc, "byte {byte} from TURN");
            assert_eq!(
                classify(&d, false),
                Verdict::Quic(QuicKind::Short),
                "byte {byte} from anyone else"
            );
        }
        // Outside 64-79 the flag changes nothing.
        for byte in (0..64u8).chain(80..=u8::MAX) {
            let mut d = vec![0u8; 1300];
            d[0] = byte;
            assert_eq!(classify(&d, true), classify(&d, false), "byte {byte}");
        }
    }

    pub(crate) fn stun_binding_request(extra_attr_words: usize) -> Vec<u8> {
        let body = extra_attr_words * 4;
        let mut d = vec![0u8; 20 + body];
        d[0] = 0x00;
        d[1] = 0x01;
        d[2..4].copy_from_slice(&u16::try_from(body).unwrap().to_be_bytes());
        d[4..8].copy_from_slice(&STUN_MAGIC_COOKIE);
        d
    }

    fn long_header(first: u8, version: u32, dcid: usize, scid: usize, len: usize) -> Vec<u8> {
        let mut d = vec![0u8; len];
        d[0] = first;
        d[1..5].copy_from_slice(&version.to_be_bytes());
        d[5] = u8::try_from(dcid).unwrap();
        d[6 + dcid] = u8::try_from(scid).unwrap();
        d
    }

    #[test]
    fn stun_needs_cookie_and_exact_length() {
        assert_eq!(classify(&stun_binding_request(0), false), Verdict::Rtc);
        assert_eq!(classify(&stun_binding_request(3), false), Verdict::Rtc);
        let mut no_cookie = stun_binding_request(1);
        no_cookie[5] = 0;
        assert_eq!(
            classify(&no_cookie, false),
            Verdict::Drop(DropReason::BadStun)
        );
        let mut wrong_len = stun_binding_request(1);
        wrong_len.push(0);
        assert_eq!(
            classify(&wrong_len, false),
            Verdict::Drop(DropReason::BadStun)
        );
        assert_eq!(
            classify(&[0u8; 19], false),
            Verdict::Drop(DropReason::BadStun)
        );
    }

    #[test]
    fn dtls_and_rtp_need_their_headers() {
        let mut client_hello = vec![0u8; 100];
        client_hello[0] = 22;
        assert_eq!(classify(&client_hello, false), Verdict::Rtc);
        assert_eq!(
            classify(&[22u8; 12], false),
            Verdict::Drop(DropReason::BadDtls)
        );
        assert_eq!(classify(&[0x2f, 0, 0, 0, 0], false), Verdict::Rtc);
        let mut rtp = vec![0u8; 172];
        rtp[0] = 0x80;
        rtp[1] = 96;
        assert_eq!(classify(&rtp, false), Verdict::Rtc);
        let mut sender_report = vec![0u8; 8];
        sender_report[0] = 0x81;
        sender_report[1] = 200;
        assert_eq!(classify(&sender_report, false), Verdict::Rtc);
        assert_eq!(
            classify(&[0x80u8; 7], false),
            Verdict::Drop(DropReason::BadRtp)
        );
    }

    #[test]
    fn quic_v1_initial_must_be_padded() {
        let ok = long_header(0xc3, 1, 8, 8, 1200);
        assert_eq!(classify(&ok, false), Verdict::Quic(QuicKind::Initial));
        let short = long_header(0xc3, 1, 8, 8, 1199);
        assert_eq!(
            classify(&short, false),
            Verdict::Drop(DropReason::ShortInitial)
        );
        // Handshake and 0-RTT have no padding rule.
        let handshake = long_header(0xe0, 1, 8, 8, 60);
        assert_eq!(classify(&handshake, false), Verdict::Quic(QuicKind::Long));
        let zero_rtt = long_header(0xd0, 1, 8, 8, 60);
        assert_eq!(classify(&zero_rtt, false), Verdict::Quic(QuicKind::Long));
    }

    #[test]
    fn quic_version_negotiation_is_dropped_in_both_ranges() {
        // VN fixes only the top bit (RFC 8999 section 5.3). In 192-255 the
        // classifier sees version 0 and drops it.
        let vn = long_header(0xc0, 0, 8, 8, 40);
        assert_eq!(
            classify(&vn, false),
            Verdict::Drop(DropReason::VersionNegotiation)
        );
        // In 128-191 the table says RTP; it is delivered to the RTC lane, never
        // to QUIC. str0m rejects it as belonging to no session (S4 sweep).
        let vn_low = long_header(0x80, 0, 8, 8, 40);
        assert_eq!(classify(&vn_low, false).lane(), Some(Lane::Rtc));
        assert_ne!(classify(&vn_low, false).lane(), Some(Lane::Quic));
    }

    #[test]
    fn quic_retry_is_dropped_at_a_server() {
        let retry = long_header(0xf0, 1, 8, 8, 60);
        assert_eq!(classify(&retry, false), Verdict::Drop(DropReason::Retry));
    }

    #[test]
    fn quic_v2_is_a_version_negotiation_trigger_and_must_be_padded() {
        // noq 1.3.5 does not speak v2 (RFC 9369). Its packet types are
        // rotated (Initial = 0b01), but as an unsupported version only the size
        // matters: >= 1200 may draw one ~60-byte VN, smaller is dropped.
        let v2_initial = long_header(0xd0, QUIC_V2, 8, 8, 1200);
        assert_eq!(
            classify(&v2_initial, false),
            Verdict::Quic(QuicKind::VersionNegotiationTrigger)
        );
        let v2_small = long_header(0xd0, QUIC_V2, 8, 8, 300);
        assert_eq!(
            classify(&v2_small, false),
            Verdict::Drop(DropReason::ShortVersionNegotiationTrigger)
        );
        let grease = long_header(0xc0, 0x1a2a_3a4a, 8, 8, 1252);
        assert_eq!(
            classify(&grease, false),
            Verdict::Quic(QuicKind::VersionNegotiationTrigger)
        );
    }

    #[test]
    fn quic_coalesced_packets_follow_the_first_packet() {
        // Initial (padded to the end of the datagram) after a Handshake: one
        // datagram, two packets (RFC 9000 section 12.2). The first packet's
        // header decides; noq splits the rest.
        let mut d = long_header(0xe0, 1, 8, 8, 1200);
        let second = 60;
        d[second] = 0xc0;
        d[second + 1..second + 5].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(classify(&d, false), Verdict::Quic(QuicKind::Long));
        // A coalesced datagram led by an Initial must still be >= 1200.
        let small = long_header(0xc0, 1, 8, 8, 400);
        assert_eq!(
            classify(&small, false),
            Verdict::Drop(DropReason::ShortInitial)
        );
    }

    #[test]
    fn quic_stateless_reset_shape_is_a_short_header() {
        // A stateless reset looks like a short header: 0b01xxxxxx, >= 21 bytes,
        // ending in a 16-byte token (RFC 9000 section 10.3).
        let mut reset = vec![0x5au8; 21];
        reset[0] = 0x4f;
        assert_eq!(classify(&reset, false), Verdict::Quic(QuicKind::Short));
        assert_eq!(
            classify(&reset[..20], false),
            Verdict::Drop(DropReason::ShortQuicShort)
        );
    }

    #[test]
    fn an_undecodable_unknown_version_never_spends_the_vn_budget() {
        let mut d = long_header(0xc0, 0x5a5a_5a5a, 8, 8, 1300);
        assert_eq!(
            classify(&d, false),
            Verdict::Quic(QuicKind::VersionNegotiationTrigger)
        );
        d[5] = 255;
        assert_eq!(classify(&d, false), Verdict::Drop(DropReason::BadQuicLong));
    }

    #[test]
    fn quic_long_header_cid_lengths_are_checked() {
        let mut bad = long_header(0xe0, 1, 8, 8, 60);
        bad[5] = 21;
        assert_eq!(
            classify(&bad, false),
            Verdict::Drop(DropReason::BadQuicLong)
        );
        assert_eq!(
            classify(&[0xc0, 0, 0, 0], false),
            Verdict::Drop(DropReason::BadQuicLong)
        );
    }

    #[test]
    fn stun_and_vn_shaped_rtp_helpers() {
        assert!(is_stun(&stun_binding_request(2)));
        let mut dtls = vec![0u8; 40];
        dtls[0] = 22;
        assert!(!is_stun(&dtls));
        for first in 128..=191u8 {
            let mut vn = vec![0u8; 40];
            vn[0] = first;
            assert!(is_vn_shaped_rtp(&vn), "{first}");
            vn[2] = 96;
            assert!(!is_vn_shaped_rtp(&vn));
        }
        let mut rtp = vec![0u8; 172];
        rtp[0] = 0x80;
        rtp[1] = 96;
        assert!(!is_vn_shaped_rtp(&rtp), "ordinary RTP");
        let mut vn_long = vec![0u8; 40];
        vn_long[0] = 0xc0;
        assert!(
            !is_vn_shaped_rtp(&vn_long),
            "192-255 is the long-header path"
        );
    }

    #[test]
    fn reserved_and_empty_are_dropped() {
        for byte in 4..=19u8 {
            let mut d = vec![0u8; 100];
            d[0] = byte;
            assert_eq!(classify(&d, false), Verdict::Drop(DropReason::Reserved));
        }
        assert_eq!(classify(&[], false), Verdict::Drop(DropReason::Empty));
    }

    #[test]
    fn source_keys_fold_mapped_v4_and_v6_slash_64() {
        let v4: SocketAddr = "192.0.2.7:5000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.0.2.7]:6000".parse().unwrap();
        assert_eq!(SourceKey::of(v4), SourceKey::of(mapped));
        let a: SocketAddr = "[2001:db8:1:2::1]:1".parse().unwrap();
        let b: SocketAddr = "[2001:db8:1:2:ffff::9]:2".parse().unwrap();
        let c: SocketAddr = "[2001:db8:1:3::1]:1".parse().unwrap();
        assert_eq!(SourceKey::of(a), SourceKey::of(b));
        assert_ne!(SourceKey::of(a), SourceKey::of(c));
        assert_eq!(canonical(mapped), "192.0.2.7:6000".parse().unwrap());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4096))]

        /// Random datagrams: whatever the classifier delivers is well formed for
        /// the lane it goes to, so garbage never reaches str0m or noq as
        /// something it is not.
        #[test]
        fn random_datagrams_reach_a_lane_only_when_well_formed(
            data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..1500),
            turn in proptest::prelude::any::<bool>(),
        ) {
            match classify(&data, turn) {
                Verdict::Rtc => {
                    let first = data[0];
                    let ok = match first_byte_class(first) {
                        ByteClass::Stun => is_well_formed_stun(&data),
                        ByteClass::Dtls => is_plausible_dtls(&data),
                        ByteClass::Rtp => data.len() >= 8,
                        ByteClass::ChannelDataOrQuicShort => turn && data.len() >= 4,
                        _ => false,
                    };
                    proptest::prop_assert!(ok, "rtc got {:?}", &data[..data.len().min(8)]);
                }
                Verdict::Quic(kind @ (QuicKind::Long | QuicKind::Initial)) => {
                    proptest::prop_assert!(data[0] >= 192);
                    let v = long_header_version(&data).unwrap();
                    proptest::prop_assert!(SUPPORTED_QUIC_VERSIONS.contains(&v));
                    // Never a Retry, never an unpadded Initial; Initials are
                    // always tagged as such (they pay the handshake budget).
                    let ty = (data[0] & 0x30) >> 4;
                    proptest::prop_assert!(ty != 3);
                    proptest::prop_assert_eq!(ty == 0, kind == QuicKind::Initial);
                    proptest::prop_assert!(ty != 0 || data.len() >= MIN_INITIAL_DATAGRAM);
                }
                Verdict::Quic(QuicKind::VersionNegotiationTrigger) => {
                    proptest::prop_assert!(data[0] >= 192);
                    proptest::prop_assert!(data.len() >= MIN_INITIAL_DATAGRAM);
                }
                Verdict::Quic(QuicKind::Short) => {
                    proptest::prop_assert!((64..=127).contains(&data[0]));
                    proptest::prop_assert!(!(turn && data[0] <= 79));
                    proptest::prop_assert!(data.len() >= MIN_SHORT_HEADER);
                }
                Verdict::Drop(_) => {}
            }
        }

        /// Uniform random datagrams almost never look like a well-formed STUN
        /// header; this pins that the cookie check holds for any prefix.
        #[test]
        fn stun_without_cookie_is_never_rtc(
            mut data in proptest::collection::vec(proptest::prelude::any::<u8>(), 20..600),
        ) {
            data[0] &= 0x03;
            if data[4..8] != STUN_MAGIC_COOKIE {
                proptest::prop_assert_eq!(classify(&data, false), Verdict::Drop(DropReason::BadStun));
            }
        }
    }
}
