//! Failing-first tests for the STUN/TURN message codec (RFC 5389 / 5766 / 8656).
//!
//! These exercise the pure wire-format encode/decode the in-crate TURN client is
//! built on: STUN headers + the magic cookie, XOR-MAPPED / XOR-RELAYED /
//! XOR-PEER address `XOR`ing (v4 + v6), MESSAGE-INTEGRITY (HMAC-SHA1 keyed by the
//! long-term key), FINGERPRINT (CRC-32), and the TURN-specific attributes
//! (REQUESTED-TRANSPORT, LIFETIME, DATA, CHANNEL-NUMBER, NONCE, REALM, ERROR-CODE).
//! No socket, no str0m — the codec is offline-testable.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use multiview_webrtc::turn::message::{
    long_term_key, Attribute, Class, Method, StunMessage, MAGIC_COOKIE,
};

#[test]
fn header_roundtrips_method_class_and_magic_cookie() {
    let msg = StunMessage::request(Method::Allocate);
    let bytes = msg.to_bytes(None);
    // STUN header is 20 bytes minimum.
    assert!(bytes.len() >= 20);
    // Bytes 4..8 are the magic cookie 0x2112A442 (RFC 5389 §6).
    assert_eq!(&bytes[4..8], &MAGIC_COOKIE.to_be_bytes());
    // The transaction id (bytes 8..20) is non-zero (random).
    assert_ne!(&bytes[8..20], &[0u8; 12]);

    let parsed = StunMessage::parse(&bytes).expect("parses its own bytes");
    assert_eq!(parsed.method(), Method::Allocate);
    assert_eq!(parsed.class(), Class::Request);
    assert_eq!(parsed.transaction_id(), msg.transaction_id());
}

#[test]
fn requested_transport_and_lifetime_attributes_roundtrip() {
    let mut msg = StunMessage::request(Method::Allocate);
    msg.push(Attribute::RequestedTransportUdp);
    msg.push(Attribute::Lifetime(600));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert!(parsed
        .attributes()
        .iter()
        .any(|a| matches!(a, Attribute::RequestedTransportUdp)));
    assert_eq!(parsed.lifetime(), Some(600));
}

#[test]
fn requested_address_family_ipv6_attribute_roundtrips() {
    // Defect D2 (IPv6-first TURN): the Allocate request must carry
    // REQUESTED-ADDRESS-FAMILY (RFC 8656 §14.7, attribute 0x0017) so the server
    // allocates an IPv6 relay. Encode + decode it.
    use multiview_webrtc::turn::message::AddressFamily;
    let mut msg = StunMessage::request(Method::Allocate);
    msg.push(Attribute::RequestedAddressFamily(AddressFamily::Ipv6));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert!(
        parsed
            .attributes()
            .iter()
            .any(|a| matches!(a, Attribute::RequestedAddressFamily(AddressFamily::Ipv6))),
        "REQUESTED-ADDRESS-FAMILY=IPv6 survives the round-trip: {:?}",
        parsed.attributes()
    );
    assert_eq!(parsed.requested_address_family(), Some(AddressFamily::Ipv6));
}

#[test]
fn requested_address_family_ipv4_attribute_roundtrips() {
    use multiview_webrtc::turn::message::AddressFamily;
    let mut msg = StunMessage::request(Method::Allocate);
    msg.push(Attribute::RequestedAddressFamily(AddressFamily::Ipv4));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert_eq!(parsed.requested_address_family(), Some(AddressFamily::Ipv4));
}

#[test]
fn xor_mapped_address_roundtrips_ipv4() {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 15), 50000));
    let mut msg = StunMessage::success(Method::Binding);
    msg.push(Attribute::XorMappedAddress(addr));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert_eq!(parsed.mapped_address(), Some(addr));
}

#[test]
fn xor_relayed_address_roundtrips_ipv6() {
    // IPv6-first: the relay address the TURN server allocates is commonly v6.
    let addr = SocketAddr::V6(SocketAddrV6::new(
        "2001:db8::15".parse::<Ipv6Addr>().unwrap(),
        49152,
        0,
        0,
    ));
    let mut msg = StunMessage::success(Method::Allocate);
    msg.push(Attribute::XorRelayedAddress(addr));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert_eq!(parsed.relayed_address(), Some(addr));
}

#[test]
fn message_integrity_verifies_with_the_right_key_and_fails_with_a_wrong_one() {
    let key = long_term_key("alice", "example.org", "s3cret");
    let wrong = long_term_key("alice", "example.org", "guess");
    let mut msg = StunMessage::request(Method::Refresh);
    msg.push(Attribute::Lifetime(0));
    msg.push(Attribute::Username("alice".to_owned()));
    msg.push(Attribute::Realm("example.org".to_owned()));
    msg.push(Attribute::Nonce("abc123".to_owned()));
    let bytes = msg.to_bytes(Some(&key));

    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert!(parsed.verify_integrity(&key), "right key must verify");
    assert!(
        !parsed.verify_integrity(&wrong),
        "wrong key must NOT verify"
    );
}

#[test]
fn fingerprint_is_present_and_correct() {
    let msg = StunMessage::request(Method::Allocate);
    let bytes = msg.to_bytes(None);
    // The last attribute must be FINGERPRINT (type 0x8028), and re-parsing must
    // validate it (a corrupted body fails parse).
    let parsed = StunMessage::parse(&bytes).expect("valid fingerprint parses");
    assert!(parsed.has_valid_fingerprint());

    let mut corrupted = bytes.clone();
    // Flip a byte in the transaction id; FINGERPRINT (CRC over the message) must
    // now fail validation.
    corrupted[10] ^= 0xFF;
    assert!(
        StunMessage::parse(&corrupted).is_err()
            || !StunMessage::parse(&corrupted)
                .unwrap()
                .has_valid_fingerprint()
    );
}

#[test]
fn error_code_response_parses_code_and_reason() {
    let mut msg = StunMessage::error(Method::Allocate);
    msg.push(Attribute::ErrorCode {
        code: 401,
        reason: "Unauthorized".to_owned(),
    });
    msg.push(Attribute::Realm("example.org".to_owned()));
    msg.push(Attribute::Nonce("xyznonce".to_owned()));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert_eq!(parsed.class(), Class::Error);
    assert_eq!(parsed.error_code(), Some(401));
    assert_eq!(parsed.realm(), Some("example.org"));
    assert_eq!(parsed.nonce(), Some("xyznonce"));
}

#[test]
fn data_indication_carries_peer_and_payload() {
    let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 9000));
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let mut msg = StunMessage::indication(Method::Data);
    msg.push(Attribute::XorPeerAddress(peer));
    msg.push(Attribute::Data(payload.clone()));
    let bytes = msg.to_bytes(None);
    let parsed = StunMessage::parse(&bytes).expect("parses");
    assert_eq!(parsed.peer_address(), Some(peer));
    assert_eq!(parsed.data(), Some(payload.as_slice()));
}

#[test]
fn parse_rejects_non_stun_first_two_bits() {
    // RFC 5389: the two most-significant bits of a STUN message must be zero.
    let not_stun = [
        0xC0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    assert!(StunMessage::parse(&not_stun).is_err());
}

#[test]
fn channel_data_is_distinguished_from_stun() {
    // ChannelData (RFC 5766 §11.4) has a channel number 0x4000..=0x7FFF in the
    // first two bytes — NOT a STUN message. The codec must classify it.
    let mut frame = vec![0x40, 0x01, 0x00, 0x04];
    frame.extend_from_slice(&[1, 2, 3, 4]);
    assert!(multiview_webrtc::turn::message::is_channel_data(&frame));
    assert!(!multiview_webrtc::turn::message::is_channel_data(
        &StunMessage::request(Method::Allocate).to_bytes(None)
    ));
}
