//! Property tests for the SNMP BER encoder invariants.
//!
//! Gated on the `snmp` feature. These check the *structural* BER laws that a
//! conforming NMS decoder relies on: minimal integer length, faithful length
//! self-description, and OID/var-bind framing — by re-parsing the bytes the
//! encoder produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "snmp")]

use multiview_telemetry::snmp::{
    encode_integer, encode_length, encode_oid, encode_trap_v2c_message, Oid, Trap, VarBind,
    VarBindValue,
};
use proptest::prelude::*;

/// Decode a BER definite length at the start of `bytes`, returning
/// `(length, header_byte_count)`.
fn decode_length(bytes: &[u8]) -> (usize, usize) {
    let first = bytes[0];
    if first < 0x80 {
        return (usize::from(first), 1);
    }
    let n = usize::from(first & 0x7F);
    let mut len = 0usize;
    for i in 0..n {
        len = (len << 8) | usize::from(bytes[1 + i]);
    }
    (len, 1 + n)
}

/// Decode a minimal two's-complement INTEGER body of `len` bytes into i64.
fn decode_int_body(body: &[u8]) -> i64 {
    let mut acc: i64 = if body[0] & 0x80 != 0 { -1 } else { 0 };
    for &b in body {
        acc = (acc << 8) | i64::from(b);
    }
    acc
}

proptest! {
    /// `encode_length` round-trips: decoding the bytes it produces yields the
    /// original length, consuming exactly the bytes it wrote.
    #[test]
    fn length_round_trips(len in 0usize..1_000_000) {
        let encoded = encode_length(len);
        let (decoded, consumed) = decode_length(&encoded);
        prop_assert_eq!(decoded, len);
        prop_assert_eq!(consumed, encoded.len());
    }

    /// `encode_integer` is a valid INTEGER TLV whose body is *minimal* (no
    /// redundant leading sign byte) and which decodes back to the input.
    #[test]
    fn integer_round_trips_and_is_minimal(value in any::<i32>()) {
        let tlv = encode_integer(i64::from(value));
        prop_assert_eq!(tlv[0], 0x02, "INTEGER tag");
        let (len, hdr) = decode_length(&tlv[1..]);
        let body = &tlv[1 + hdr..];
        prop_assert_eq!(body.len(), len, "length self-describes the body");
        prop_assert_eq!(decode_int_body(body), i64::from(value));
        // Minimality (X.690 §8.3.2): the first two octets are never a redundant
        // sign extension.
        if body.len() > 1 {
            let redundant_zero = body[0] == 0x00 && (body[1] & 0x80) == 0;
            let redundant_ones = body[0] == 0xFF && (body[1] & 0x80) != 0;
            prop_assert!(!redundant_zero && !redundant_ones, "non-minimal INTEGER {body:?}");
        }
    }

    /// `encode_oid` always begins with the OID tag and a length that exactly
    /// describes the remaining content, for any well-formed arc.
    #[test]
    fn oid_is_well_framed(
        a in 0u32..3,
        b in 0u32..40,
        tail in proptest::collection::vec(0u32..200_000, 0..6),
    ) {
        let mut arc = vec![a, b];
        arc.extend(tail);
        let oid = Oid::new(arc).unwrap();
        let bytes = encode_oid(&oid);
        prop_assert_eq!(bytes[0], 0x06, "OID tag");
        let (len, hdr) = decode_length(&bytes[1..]);
        prop_assert_eq!(bytes.len(), 1 + hdr + len, "OID length frames the content");
        // Every continuation byte but the terminator of each sub-id has bit 7
        // set; the final content octet of the whole OID must be a terminator
        // (bit 7 clear).
        let content = &bytes[1 + hdr..];
        prop_assert!(content.last().is_some_and(|b| b & 0x80 == 0));
    }

    /// A full SNMPv2c trap message is a single SEQUENCE whose declared length
    /// exactly covers the rest of the datagram (a decoder reads no trailing
    /// garbage and never runs short).
    #[test]
    fn trap_message_sequence_length_is_exact(
        request_id in any::<i32>(),
        up in any::<u32>(),
        sev in -10i32..10,
    ) {
        let trap = Trap::new(
            up,
            Oid::new(vec![1, 3, 6, 1, 4, 1, 99999, 0, 1]).unwrap(),
            vec![VarBind::new(
                Oid::new(vec![1, 3, 6, 1, 4, 1, 99999, 1, 1]).unwrap(),
                VarBindValue::Integer { value: sev },
            )],
        );
        let pdu = trap.to_pdu(request_id);
        let msg = encode_trap_v2c_message(1, "public", &pdu);
        prop_assert_eq!(msg[0], 0x30, "top-level SEQUENCE");
        let (len, hdr) = decode_length(&msg[1..]);
        prop_assert_eq!(msg.len(), 1 + hdr + len, "SEQUENCE length frames the message");
    }
}
