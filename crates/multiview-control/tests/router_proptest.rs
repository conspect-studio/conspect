//! Property + golden-vector tests for the pure router protocol codecs
//! (SW-P-08 and Ember+ Glow/S101) and the route-follow tally logic.
//!
//! These pin the wire round-trip invariants a real router integration depends on,
//! exhaustively over generated inputs — no sockets, no async.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::router::ember::{self, ber, s101, GlowNode, GlowValue};
use multiview_control::router::swp08::{self, SwP08Message, MAX_ADDRESS};
use multiview_control::{route_follow, RouteBinding, RouterRoute};
use multiview_core::tally::TallyColor;
use multiview_events::TallyTarget;
use proptest::prelude::*;

/// A SW-P-08 address within the protocol's 0..=1023 range.
fn any_address() -> impl Strategy<Value = u16> {
    0u16..=MAX_ADDRESS
}

/// A matrix/level nibble (0..=15).
fn any_nibble() -> impl Strategy<Value = u16> {
    0u16..=0x0F
}

/// Any modelled SW-P-08 message with in-range fields.
fn any_swp08() -> impl Strategy<Value = SwP08Message> {
    prop_oneof![
        (any_nibble(), any_nibble(), any_address()).prop_map(|(matrix, level, destination)| {
            SwP08Message::Interrogate {
                matrix,
                level,
                destination,
            }
        }),
        (any_nibble(), any_nibble(), any_address(), any_address()).prop_map(
            |(matrix, level, destination, source)| SwP08Message::Connect {
                matrix,
                level,
                destination,
                source,
            }
        ),
        (any_nibble(), any_nibble(), any_address(), any_address()).prop_map(
            |(matrix, level, destination, source)| SwP08Message::Connected {
                matrix,
                level,
                destination,
                source,
            }
        ),
    ]
}

proptest! {
    /// Every modelled SW-P-08 message survives the full framed wire round-trip
    /// (encode_message → decode_message) exactly.
    #[test]
    fn swp08_message_round_trips(msg in any_swp08()) {
        let frame = swp08::encode_message(&msg).expect("in-range encodes");
        let back = swp08::decode_message(&frame).expect("frame decodes");
        prop_assert_eq!(back, msg);
    }

    /// A SW-P-08 frame's BCC validates the body: (sum + bcc) & 0xFF == 0 for any
    /// generated body, so corrupting any single byte is detectable.
    #[test]
    fn swp08_bcc_zeroes_the_body_sum(body in proptest::collection::vec(any::<u8>(), 0..32)) {
        let bcc = swp08::bcc(&body);
        let sum = body.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        prop_assert_eq!(sum.wrapping_add(bcc), 0);
    }

    /// A SW-P-08 frame round-trips even when the body is full of DLE bytes that
    /// must be byte-stuffed and recovered.
    #[test]
    fn swp08_frame_unstuffs_any_body(body in proptest::collection::vec(any::<u8>(), 0..48)) {
        let frame = swp08::encode_frame(&body);
        let back = swp08::decode_frame(&frame).expect("frame decodes");
        prop_assert_eq!(back, body);
    }

    /// A signed integer survives the BER INTEGER round-trip, including the
    /// minimal-encoding trimming for any i64.
    #[test]
    fn ber_integer_round_trips(value in any::<i64>()) {
        let encoded = ber::encode_integer(value);
        let (tag, body, _) = ber::decode_tlv(&encoded).expect("tlv decodes");
        prop_assert_eq!(tag, ber::TAG_INTEGER);
        prop_assert_eq!(ber::decode_integer(body).expect("integer decodes"), value);
    }

    /// A BER length prefix round-trips for any usize.
    #[test]
    fn ber_length_round_trips(len in 0usize..1_000_000) {
        let encoded = ber::encode_length(len);
        let (decoded, rest) = ber::decode_length(&encoded).expect("length decodes");
        prop_assert_eq!(decoded, len);
        prop_assert!(rest.is_empty());
    }

    /// An arbitrary byte payload survives the S101 frame round-trip, even when it
    /// is full of delimiter/escape bytes — and the CRC catches a flipped byte.
    #[test]
    fn s101_frame_round_trips_any_payload(
        payload in proptest::collection::vec(any::<u8>(), 0..64)
    ) {
        let frame = s101::encode(&payload);
        let back = s101::decode(&frame).expect("frame decodes");
        prop_assert_eq!(&back, &payload);

        // Flipping a non-delimiter payload byte must be caught by the CRC. Find a
        // body position that is a raw (non-escaped) byte to flip.
        if !payload.is_empty() {
            let mut corrupt = frame.clone();
            // Frame body starts at index 1 (after BOF). Flip the first body byte.
            corrupt[1] ^= 0x01;
            // Either CRC fails or (rarely) it un-escapes to a different valid
            // payload; in both cases it must NOT equal the original payload.
            if let Ok(other) = s101::decode(&corrupt) {
                prop_assert_ne!(other, payload);
            }
        }
    }

    /// A Glow node (string-valued parameter) survives the full S101 round-trip.
    #[test]
    fn glow_string_node_round_trips(
        number in any::<i64>(),
        identifier in "[a-zA-Z0-9_-]{1,24}",
        label in "[ -~]{0,40}",
    ) {
        let node = GlowNode {
            number,
            identifier,
            value: Some(GlowValue::string(label)),
        };
        let frame = node.encode_message();
        let back = GlowNode::decode_message(&frame).expect("node decodes");
        prop_assert_eq!(back, node);
    }

    /// A Glow node (integer-valued parameter) survives the full S101 round-trip.
    #[test]
    fn glow_integer_node_round_trips(
        number in any::<i64>(),
        identifier in "[a-zA-Z0-9_-]{1,24}",
        value in any::<i64>(),
    ) {
        let node = GlowNode {
            number,
            identifier,
            value: Some(GlowValue::integer(value)),
        };
        let frame = node.encode_message();
        let back = GlowNode::decode_message(&frame).expect("node decodes");
        prop_assert_eq!(back, node);
    }

    /// Route-follow lights the program lamp exactly for a program source routed
    /// to a watched destination, and never for an unwatched destination.
    #[test]
    fn route_follow_classifies_program_sources(
        source in 0u16..50,
        destination in 0u16..10,
    ) {
        let binding = RouteBinding {
            level: 1,
            destination: 5,
            target: TallyTarget::Tile { index: 3 },
            program_sources: vec![10, 11, 12],
            preview_sources: vec![20],
        };
        let route = RouterRoute { level: 1, destination, source };
        let result = route_follow(route, &binding);
        if destination == 5 {
            let update = result.expect("a watched destination yields an update");
            let expected = if (10..=12).contains(&source) {
                TallyColor::Red
            } else if source == 20 {
                TallyColor::Green
            } else {
                TallyColor::Off
            };
            prop_assert_eq!(update.color, expected);
        } else {
            prop_assert!(result.is_none());
        }
    }
}

// ---- Golden vectors (fixed inputs → fixed bytes) ----

#[test]
fn swp08_connect_golden_vector() {
    // A Connect on matrix 1, level 2, dest 3, source 4. Body is:
    //   CMD_CONNECT(0x02) | matrix<<4|level (0x12) | dest hi/lo (0x00,0x03) |
    //   source hi/lo (0x00,0x04)
    let msg = SwP08Message::Connect {
        matrix: 1,
        level: 2,
        destination: 3,
        source: 4,
    };
    let body = msg.encode_body().expect("encodes");
    assert_eq!(body, vec![0x02, 0x12, 0x00, 0x03, 0x00, 0x04]);
    // Framed: DLE STX <body> DLE ETX <bcc>. BCC = two's complement of sum.
    let frame = swp08::encode_frame(&body);
    assert_eq!(&frame[0..2], &[0x10, 0x02]); // DLE STX
    assert_eq!(&frame[frame.len() - 3..frame.len() - 1], &[0x10, 0x03]); // DLE ETX
                                                                         // Round-trips back to the message.
    assert_eq!(swp08::decode_message(&frame).unwrap(), msg);
}

#[test]
fn ber_integer_golden_vectors() {
    // Pinned minimal-encoding outputs.
    assert_eq!(ber::encode_integer(0), vec![ber::TAG_INTEGER, 0x01, 0x00]);
    assert_eq!(ber::encode_integer(1), vec![ber::TAG_INTEGER, 0x01, 0x01]);
    assert_eq!(ber::encode_integer(-1), vec![ber::TAG_INTEGER, 0x01, 0xFF]);
    assert_eq!(
        ber::encode_integer(256),
        vec![ber::TAG_INTEGER, 0x02, 0x01, 0x00]
    );
}

#[test]
fn s101_crc_golden_vector() {
    // CRC-16/CCITT (X.25) of the canonical check string "123456789" is 0x906E.
    assert_eq!(s101::crc16(b"123456789"), 0x906E);
}

#[test]
fn ember_module_reexports_are_reachable() {
    // The `ember` parent module is reachable (the codecs live under it).
    let node = GlowNode {
        number: 0,
        identifier: "x".to_owned(),
        value: None,
    };
    let frame = ember::s101::encode(&node.encode_ber());
    assert!(!frame.is_empty());
}
