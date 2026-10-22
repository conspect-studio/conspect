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
use multiview_output::tsl::{self, TallyLamp, TslError, TslVersion, UmdDisplay, UmdMessage};

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
    let packet = tsl::v40::encode(&msg).expect("v4.0 encode");
    // Byte-identical to the multiview-input decoder's `v40_golden()` vector:
    // 82 33 56 54 52 [20 ×13] 2F 02 1E 1E. CHKSUM 0x2F makes the mod-128 sum
    // of bytes 0..=18 zero (849 + 47 = 896 = 7 × 128); XDATA 0x1E packs
    // LH=red, Txt=amber, RH=green.
    assert_eq!(
        packet,
        [
            0x82, 0x33, 0x56, 0x54, 0x52, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, //
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x2F, 0x02, 0x1E, 0x1E,
        ]
    );
    let sum: u32 = packet[..19].iter().map(|&b| u32::from(b)).sum();
    assert_eq!(sum % 128, 0);
}

#[test]
fn v40_encodes_all_call_and_dim() {
    // All-call address 0x7F with a dim (level 1) lit lamp. The encoder derives
    // the CTRL brightness field from the brightest LIT lamp — an all-Off
    // display would emit the FULL baseline instead, so this uses a lit lamp to
    // pin the dim path. CHKSUM: (255 + 17 + 16×32) = 784, 784 mod 128 = 16,
    // 128 − 16 = 112 = 0x70. XDATA: LH=red → 0x10.
    let mut dim = lamp(TallyColor::Red);
    dim.brightness = Brightness::new(1);
    let msg = UmdMessage {
        version: TslVersion::V40,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 0x7F,
            left: dim,
            text_tally: TallyLamp::off(),
            right: TallyLamp::off(),
            text: String::new(),
        }],
    };
    let packet = tsl::v40::encode(&msg).expect("v4.0 all-call encode");
    assert_eq!(
        packet,
        [
            0xFF, 0x11, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, //
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x70, 0x02, 0x10, 0x10,
        ]
    );
    let sum: u32 = packet[..19].iter().map(|&b| u32::from(b)).sum();
    assert_eq!(sum % 128, 0);
}

#[test]
fn v40_encodes_all_off_as_full_brightness() {
    // Contract: the CTRL brightness field is the max brightness over LIT
    // lamps; with none lit it falls back to FULL (CTRL 0x30). Unlit-lamp
    // brightness is unobservable on the wire — this vector pins the fallback
    // so a codec writing dim (0x00) for all-Off cannot ship green.
    // CHKSUM: 255 + 48 + 16×32 = 815, 815 mod 128 = 47, 128 − 47 = 81 = 0x51.
    let msg = UmdMessage {
        version: TslVersion::V40,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 0x7F,
            left: TallyLamp::off(),
            text_tally: TallyLamp::off(),
            right: TallyLamp::off(),
            text: String::new(),
        }],
    };
    let packet = tsl::v40::encode(&msg).expect("v4.0 all-off encode");
    assert_eq!(
        packet,
        [
            0xFF, 0x30, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, //
            0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x51, 0x02, 0x00, 0x00,
        ]
    );
    let sum: u32 = packet[..19].iter().map(|&b| u32::from(b)).sum();
    assert_eq!(sum % 128, 0);
}

#[test]
fn v40_rejects_overlong_label() {
    let msg = UmdMessage {
        version: TslVersion::V40,
        screen: 0,
        displays: vec![UmdDisplay {
            index: 0,
            left: TallyLamp::off(),
            text_tally: TallyLamp::off(),
            right: TallyLamp::off(),
            text: "0123456789ABCDEFG".to_owned(), // 17 chars
        }],
    };
    assert!(matches!(
        tsl::v40::encode(&msg),
        Err(TslError::LabelTooLong { len: 17, max: 16 })
    ));
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
    // CONTROL: bits 0-1 RH=red(01), bits 2-3 Text=off(00), bits 4-5 LH=green(10),
    // bits 6-7 brightness=full(11) — the bit order the TSL 5.0 spec prescribes.
    assert_eq!(control, 0xE1);
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
            index: 0x00FE, // low byte 0xFE == DLE, forces a stuffing event
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
    assert_eq!(stuffed[0], 0xFE);
    assert_eq!(stuffed[1], 0x02);
    // v5.0 TCP framing has NO terminator: stuffed = opener + payload only,
    // and the only inserted bytes are the DLE duplicates.
    let raw_dles = raw.iter().filter(|&&b| b == tsl::v50::DLE).count();
    assert_eq!(stuffed.len(), 2 + raw.len() + raw_dles);
    let body = &stuffed[2..];
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
