//! TSL UMD **encoder** tests: golden on-wire vectors (typed message → known spec
//! bytes) plus structural/error-path coverage. The golden vectors are
//! byte-identical to the ones the `multiview-input` decoder consumes, so the two
//! halves agree on the wire format.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: test fixtures hand-assemble byte vectors with small, in-range
    // length fields; `as u16` is the clearest way to write the wire count.
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    // reason: counting DLE bytes in a tiny test packet; pulling in the
    // `bytecount` crate for a handful of bytes is not worth a dependency.
    clippy::naive_bytecount
)]

use multiview_core::tally::{Brightness, TallyColor};
use multiview_output::tsl::{self, TallyLamp, TslVersion, UmdDisplay, UmdMessage};

fn lamp(color: TallyColor) -> TallyLamp {
    TallyLamp {
        color,
        brightness: Brightness::FULL,
    }
}

// ---------------------------------------------------------------------------
// v3.1
// ---------------------------------------------------------------------------

#[test]
fn v31_encodes_golden_vector() {
    let msg = UmdMessage {
        version: TslVersion::V31,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 1,
            left: lamp(TallyColor::Red),
            text_tally: TallyLamp::off(),
            right: lamp(TallyColor::Red),
            text: "CAM 1".to_owned(),
        }],
    };
    let packet = tsl::v31::encode(&msg).expect("v3.1 encode");
    assert_eq!(packet.len(), 18);
    assert_eq!(packet[0], 0x81); // sync + address 1
    assert_eq!(packet[1], (0b11 << 4) | 0b10 | 0b01); // bright 3, both tally on
    assert_eq!(&packet[2..7], b"CAM 1");
    assert!(packet[7..].iter().all(|&b| b == b' ')); // space padded
}

#[test]
fn v31_rejects_overlong_label() {
    let msg = UmdMessage {
        version: TslVersion::V31,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 0,
            left: TallyLamp::off(),
            text_tally: TallyLamp::off(),
            right: TallyLamp::off(),
            text: "0123456789ABCDEFG".to_owned(), // 17 chars
        }],
    };
    assert!(tsl::v31::encode(&msg).is_err());
}

// ---------------------------------------------------------------------------
// v4.0
// ---------------------------------------------------------------------------

#[test]
fn v40_encodes_golden_vector_with_valid_checksum() {
    let msg = UmdMessage {
        version: TslVersion::V40,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 2,
            left: lamp(TallyColor::Red),
            text_tally: lamp(TallyColor::Amber),
            right: lamp(TallyColor::Green),
            text: "VTR".to_owned(),
        }],
    };
    let core = tsl::v40::encode(&msg).expect("v4.0 encode");
    assert_eq!(core.len(), 19);
    assert_eq!(core[0], 0x82); // sync + address 2
    assert_eq!(core[1], 0b01 | (0b10 << 2) | (0b11 << 4) | (0b11 << 6));
    assert_eq!(&core[2..5], b"VTR");
    // checksum makes the whole 19-byte sum zero
    let sum = core.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    assert_eq!(sum, 0);
}

#[test]
fn v40_framed_wraps_in_dle_stx() {
    let display = UmdDisplay {
        index: 2,
        left: lamp(TallyColor::Red),
        text_tally: lamp(TallyColor::Amber),
        right: lamp(TallyColor::Green),
        text: "VTR".to_owned(),
    };
    let framed = tsl::v40::encode_display_framed(&display).expect("framed");
    assert_eq!(framed[0], tsl::v40::DLE);
    assert_eq!(framed[1], tsl::v40::STX);
    assert_eq!(framed.len(), 2 + 19);
}

// ---------------------------------------------------------------------------
// v5.0
// ---------------------------------------------------------------------------

#[test]
fn v50_encodes_golden_ascii_vector() {
    let msg = UmdMessage {
        version: TslVersion::V50,
        screen: 5,
        displays: vec![UmdDisplay {
            index: 0,
            left: lamp(TallyColor::Green),
            text_tally: TallyLamp::off(),
            right: lamp(TallyColor::Red),
            text: "SRC".to_owned(),
        }],
    };
    let packet = tsl::v50::encode(&msg, false).expect("v5.0 encode");

    // PBC = bytes after the first 2.
    let pbc = u16::from_le_bytes([packet[0], packet[1]]) as usize;
    assert_eq!(pbc, packet.len() - 2);
    assert_eq!(packet[2], tsl::v50::VERSION); // VER
    assert_eq!(packet[3], 0); // FLAGS: ASCII
    assert_eq!(u16::from_le_bytes([packet[4], packet[5]]), 5); // SCREEN
    assert_eq!(u16::from_le_bytes([packet[6], packet[7]]), 0); // INDEX
    let control = u16::from_le_bytes([packet[8], packet[9]]);
    // L=green(10) R=red(01)<<2 Text=off(00) B=full(11)<<6
    assert_eq!(control, 0b10 | (0b01 << 2) | (0b11 << 6));
    let len = u16::from_le_bytes([packet[10], packet[11]]) as usize;
    assert_eq!(len, 3);
    assert_eq!(&packet[12..15], b"SRC");
}

#[test]
fn v50_unicode_sets_flag_and_utf16_text() {
    let msg = UmdMessage {
        version: TslVersion::V50,
        screen: tsl::v50::FLAG_UNICODE.into(),
        displays: vec![UmdDisplay {
            index: 0,
            left: TallyLamp::off(),
            text_tally: TallyLamp::off(),
            right: TallyLamp::off(),
            text: "café".to_owned(),
        }],
    };
    let packet = tsl::v50::encode(&msg, true).expect("v5.0 unicode encode");
    assert_eq!(packet[3] & tsl::v50::FLAG_UNICODE, tsl::v50::FLAG_UNICODE);
    let len = u16::from_le_bytes([packet[10], packet[11]]) as usize;
    assert_eq!(len, "café".encode_utf16().count() * 2);
}

#[test]
fn v50_stuffed_doubles_dle_and_wraps() {
    let msg = UmdMessage {
        version: TslVersion::V50,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 0x0010, // low byte 0x10 == DLE, forces a stuffing event
            left: TallyLamp::off(),
            text_tally: TallyLamp::off(),
            right: TallyLamp::off(),
            text: "x".to_owned(),
        }],
    };
    let raw = tsl::v50::encode(&msg, false).expect("raw");
    let stuffed = tsl::v50::encode_stuffed(&msg, false).expect("stuffed");
    assert_eq!(stuffed[0], tsl::v50::DLE);
    assert_eq!(stuffed[1], tsl::v50::STX);
    assert_eq!(stuffed[stuffed.len() - 2], tsl::v50::DLE);
    assert_eq!(stuffed[stuffed.len() - 1], tsl::v50::ETX);
    // every literal DLE in the raw payload appears doubled in the stuffed body
    let raw_dles = raw.iter().filter(|&&b| b == tsl::v50::DLE).count();
    // body between the 2-byte opener and 2-byte closer
    let body = &stuffed[2..stuffed.len() - 2];
    let body_dles = body.iter().filter(|&&b| b == tsl::v50::DLE).count();
    assert_eq!(body_dles, raw_dles * 2);
}

#[test]
fn v50_rejects_empty_message() {
    let msg = UmdMessage {
        version: TslVersion::V50,
        screen: 0,
        displays: vec![],
    };
    assert!(tsl::v50::encode(&msg, false).is_err());
}
