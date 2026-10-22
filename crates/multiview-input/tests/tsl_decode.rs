//! TSL UMD **decoder** tests: golden on-wire vectors (decode known spec bytes
//! into typed messages) plus structural/error-path coverage.
//!
//! These run in the DEFAULT (pure-Rust) build — the codecs are socket-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: test fixtures hand-assemble byte vectors with small, statically
    // in-range length fields; `as u16` is the natural way to write the wire
    // count and cannot truncate for these tiny test packets.
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_core::tally::{Brightness, TallyColor};
use multiview_input::tsl::{self, TslError, TslVersion};

// ---------------------------------------------------------------------------
// v3.1 — 18-byte fixed packet
// ---------------------------------------------------------------------------

/// Build a v3.1 golden packet: address 1, both tally bits set, full brightness,
/// label "CAM 1".
fn v31_golden() -> Vec<u8> {
    let mut p = vec![0u8; 18];
    p[0] = 0x80 | 0x01; // sync + address 1
                        // brightness 3 (bits 4-5) | tally2 (right) | tally1 (left)
    p[1] = (0b11 << 4) | 0b10 | 0b01;
    let label = b"CAM 1";
    p[2..2 + label.len()].copy_from_slice(label);
    for b in &mut p[2 + label.len()..] {
        *b = b' ';
    }
    p
}

#[test]
fn v31_decodes_golden_vector() {
    let msg = tsl::v31::decode(&v31_golden()).expect("v3.1 decode");
    assert_eq!(msg.version, TslVersion::V31);
    assert_eq!(msg.screen, 0);
    assert_eq!(msg.displays.len(), 1);
    let d = &msg.displays[0];
    assert_eq!(d.index, 1);
    assert_eq!(d.text, "CAM 1");
    // v3.1 on/off tally => lit lamps are Red, text tally always off.
    assert_eq!(d.left.color, TallyColor::Red);
    assert_eq!(d.right.color, TallyColor::Red);
    assert_eq!(d.text_tally.color, TallyColor::Off);
    assert_eq!(d.left.brightness, Brightness::FULL);
}

#[test]
fn v31_off_tally_decodes_off() {
    let mut p = v31_golden();
    p[1] = 0b01 << 4; // brightness 1, both tally bits clear
    let d = &tsl::v31::decode(&p).unwrap().displays[0];
    assert_eq!(d.left.color, TallyColor::Off);
    assert_eq!(d.right.color, TallyColor::Off);
    assert_eq!(d.left.brightness, Brightness::new(1));
}

#[test]
fn v31_rejects_wrong_length() {
    assert!(matches!(
        tsl::v31::decode(&[0u8; 17]),
        Err(TslError::TooShort { need: 18, got: 17 })
    ));
    assert!(matches!(
        tsl::v31::decode(&[0u8; 19]),
        Err(TslError::TooLong { max: 18, got: 19 })
    ));
}

#[test]
fn v31_rejects_missing_sync_bit() {
    let mut p = v31_golden();
    p[0] &= 0x7F; // clear sync bit
    assert!(matches!(tsl::v31::decode(&p), Err(TslError::Framing(_))));
}

// ---------------------------------------------------------------------------
// v4.0 — 22-byte packet: 18-byte v3.1 core + CHKSUM + VBC + two XDATA bytes
// ---------------------------------------------------------------------------

/// The v4.0 golden vector, hand-derived from the TSL UMD protocol spec's
/// byte-layout rules (the spec prints no worked 22-byte example): address 2,
/// full brightness, both CTRL tally outputs lit, label "VTR",
/// XDATA LH=red / Txt=amber / RH=green.
///
/// ```text
/// 82 33 56 54 52 [20 ×13] 2F 02 1E 1E
/// ```
///
/// CHKSUM 0x2F: the sum of the 18 v3.1 bytes is 849, 849 mod 128 = 81, and
/// 128 − 81 = 47 = 0x2F (849 + 47 = 896 = 7 × 128).
fn v40_golden() -> Vec<u8> {
    vec![
        0x82, 0x33, 0x56, 0x54, 0x52, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, //
        0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x2F, 0x02, 0x1E, 0x1E,
    ]
}

#[test]
fn v40_decodes_golden_vector() {
    let msg = tsl::v40::decode(&v40_golden()).expect("v4.0 decode");
    assert_eq!(msg.version, TslVersion::V40);
    assert_eq!(msg.screen, 0);
    assert_eq!(msg.displays.len(), 1);
    let d = &msg.displays[0];
    assert_eq!(d.index, 2);
    assert_eq!(d.text, "VTR");
    // Lamps come from XDATA byte 1 (Display L): LH=red, Txt=amber, RH=green.
    assert_eq!(d.left.color, TallyColor::Red);
    assert_eq!(d.text_tally.color, TallyColor::Amber);
    assert_eq!(d.right.color, TallyColor::Green);
    assert_eq!(d.left.brightness, Brightness::FULL);
}

/// Golden vector 2: the reserved all-call address `0x7F`, brightness level 1
/// (CTRL bits 5-4 = `01`), no tally outputs lit, blank label.
///
/// ```text
/// FF 10 [20 ×16] 71 02 00 00
/// ```
///
/// CHKSUM 0x71: the sum of the 18 v3.1 bytes is 783, 783 mod 128 = 15, and
/// 128 − 15 = 113 = 0x71.
#[test]
fn v40_decodes_all_call_and_dim() {
    let p = [
        0xFF, 0x10, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, //
        0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x71, 0x02, 0x00, 0x00,
    ];
    let msg = tsl::v40::decode(&p).expect("v4.0 all-call decode");
    let d = &msg.displays[0];
    assert_eq!(d.index, 0x7F);
    assert_eq!(d.text, "");
    assert_eq!(d.left.color, TallyColor::Off);
    assert_eq!(d.text_tally.color, TallyColor::Off);
    assert_eq!(d.right.color, TallyColor::Off);
    assert_eq!(d.left.brightness, Brightness::new(1));
}

#[test]
fn v40_rejects_bad_checksum() {
    let mut p = v40_golden();
    p[18] = p[18].wrapping_add(1); // corrupt CHKSUM
    assert!(matches!(
        tsl::v40::decode(&p),
        Err(TslError::Checksum {
            expected: 0x30,
            computed: 0x2F
        })
    ));
}

#[test]
fn v40_rejects_wrong_length() {
    assert!(matches!(
        tsl::v40::decode(&[0u8; 21]),
        Err(TslError::TooShort { need: 22, got: 21 })
    ));
    assert!(matches!(
        tsl::v40::decode(&[0u8; 23]),
        Err(TslError::TooLong { max: 22, got: 23 })
    ));
}

#[test]
fn v40_rejects_bad_vbc() {
    // VBC (byte 19) must be 0x02 and sits outside the checksummed range.
    let mut p = v40_golden();
    p[19] = 0x03;
    assert!(matches!(tsl::v40::decode(&p), Err(TslError::Framing(_))));
}

#[test]
fn v40_rejects_command_control() {
    // CTRL bit 6 set = command message, not display data. Setting it adds 0x40
    // to the mod-128 checksummed sum, so CHKSUM becomes 0x6F.
    let mut p = v40_golden();
    p[1] = 0x73;
    p[18] = 0x6F;
    assert!(matches!(tsl::v40::decode(&p), Err(TslError::Framing(_))));
}

#[test]
fn v40_rejects_reserved_ctrl_bit() {
    // Setting CTRL bit 7 (reserved, must be 0) adds 128 ≡ 0 (mod 128) to the
    // checksummed sum: the CHKSUM stays valid, so the failure is bit 7 itself.
    let mut p = v40_golden();
    p[1] = 0xB3;
    assert!(matches!(tsl::v40::decode(&p), Err(TslError::Framing(_))));
}

#[test]
fn v40_rejects_missing_sync_bit() {
    // Clearing the address byte's sync bit changes the checksummed sum by
    // −128 ≡ 0 (mod 128), so the CHKSUM stays valid: the failure must be the
    // sync bit itself.
    let mut p = v40_golden();
    p[0] = 0x02;
    assert!(matches!(tsl::v40::decode(&p), Err(TslError::Framing(_))));
}

#[test]
fn v40_rejects_xdata_bit7() {
    // XDATA bit 7 must be 0. Bytes 19-21 sit outside the checksummed range,
    // so setting bit 7 leaves the CHKSUM valid: the failure must be the
    // reserved bit itself, not the checksum. The message pins which XDATA
    // byte was rejected (the decoder checks byte 1 before byte 2).
    let mut p = v40_golden();
    p[20] = 0x9E; // golden 0x1E | bit 7
    p[21] = 0x00; // clean byte 2: the failure is byte 1
    assert!(matches!(
        tsl::v40::decode(&p),
        Err(TslError::Framing("v4.0 XDATA byte 1 reserved high bit set"))
    ));

    let mut p = v40_golden();
    p[21] = 0x80; // golden byte 2 is 0x1E; bit 7 alone
    assert!(matches!(
        tsl::v40::decode(&p),
        Err(TslError::Framing("v4.0 XDATA byte 2 reserved high bit set"))
    ));
}

#[test]
fn v40_ignores_xbyte2() {
    // The lamps come from XDATA byte 1 (Display L); XDATA byte 2 (Display R)
    // is carried but not authoritative. Bytes 19-21 sit outside the
    // checksummed range, so no CHKSUM update is needed.
    let mut p = v40_golden();
    p[21] = 0x24; // LH=green, Txt=red, RH=off — entirely different colours
    let msg = tsl::v40::decode(&p).expect("v4.0 decode with patched Xbyte2");
    let d = &msg.displays[0];
    assert_eq!(d.left.color, TallyColor::Red);
    assert_eq!(d.text_tally.color, TallyColor::Amber);
    assert_eq!(d.right.color, TallyColor::Green);
}

// ---------------------------------------------------------------------------
// v5.0 — variable-length IP packet
// ---------------------------------------------------------------------------

/// Build a v5.0 golden packet: screen 5, one display index 0, left=green,
/// right=red, text=off, brightness full, ASCII label "SRC".
fn v50_golden_ascii() -> Vec<u8> {
    let label = b"SRC";
    // CONTROL (little-endian on the wire): bits 0-1 RH=01(red),
    // bits 2-3 Text=00(off), bits 4-5 LH=10(green), bits 6-7 brightness=11.
    let control: u16 = 0xE1;
    let mut after_pbc = Vec::new();
    after_pbc.push(0); // VER
    after_pbc.push(0); // FLAGS (ASCII)
    after_pbc.extend_from_slice(&5u16.to_le_bytes()); // SCREEN
    after_pbc.extend_from_slice(&0u16.to_le_bytes()); // INDEX
    after_pbc.extend_from_slice(&control.to_le_bytes());
    after_pbc.extend_from_slice(&(label.len() as u16).to_le_bytes());
    after_pbc.extend_from_slice(label);
    let pbc = after_pbc.len() as u16;
    let mut packet = pbc.to_le_bytes().to_vec();
    packet.extend_from_slice(&after_pbc);
    packet
}

#[test]
fn v50_decodes_golden_ascii_vector() {
    let msg = tsl::v50::decode(&v50_golden_ascii()).expect("v5.0 decode");
    assert_eq!(msg.version, TslVersion::V50);
    assert_eq!(msg.screen, 5);
    assert_eq!(msg.displays.len(), 1);
    let d = &msg.displays[0];
    assert_eq!(d.index, 0);
    assert_eq!(d.text, "SRC");
    // CONTROL 0xE1: RH bits 0-1 = red, Text bits 2-3 = off, LH bits 4-5 =
    // green, brightness bits 6-7 = full.
    assert_eq!(d.left.color, TallyColor::Green);
    assert_eq!(d.right.color, TallyColor::Red);
    assert_eq!(d.text_tally.color, TallyColor::Off);
    assert_eq!(d.left.brightness, Brightness::FULL);
}

#[test]
fn v50_decodes_unicode_and_multi_display() {
    // Two displays, UTF-16LE text, broadcast screen.
    // v5.0 text is exact-length (NOT space-padded/trimmed like v3.1/v4.0): the
    // leading space here must round-trip verbatim.
    let l0 = " café".encode_utf16().collect::<Vec<u16>>();
    let l0_expected = " café";
    let l1 = "日本".encode_utf16().collect::<Vec<u16>>();
    let bytes16 =
        |units: &[u16]| -> Vec<u8> { units.iter().flat_map(|u| u.to_le_bytes()).collect() };
    let mut after_pbc = Vec::new();
    after_pbc.push(0);
    after_pbc.push(tsl::v50::FLAG_UNICODE);
    after_pbc.extend_from_slice(&0xFFFFu16.to_le_bytes()); // broadcast screen
    for (idx, units) in [(0u16, &l0), (1u16, &l1)] {
        let text = bytes16(units);
        after_pbc.extend_from_slice(&idx.to_le_bytes());
        after_pbc.extend_from_slice(&0u16.to_le_bytes()); // control: all off
        after_pbc.extend_from_slice(&(text.len() as u16).to_le_bytes());
        after_pbc.extend_from_slice(&text);
    }
    let mut packet = (after_pbc.len() as u16).to_le_bytes().to_vec();
    packet.extend_from_slice(&after_pbc);

    let msg = tsl::v50::decode(&packet).expect("v5.0 unicode decode");
    assert_eq!(msg.screen, tsl::v50::BROADCAST);
    assert_eq!(msg.displays.len(), 2);
    assert_eq!(msg.displays[0].text, l0_expected);
    assert_eq!(msg.displays[1].text, "日本");
}

#[test]
fn v50_skips_control_data_dmsgs() {
    // DMSGs with CONTROL bit 15 set are control-data (device-specific) and
    // must be skipped: here a control-data DMSG for index 1 (zero-length
    // text) is followed by a display DMSG for index 2.
    let mut after_pbc = Vec::new();
    after_pbc.push(0); // VER
    after_pbc.push(0); // FLAGS (ASCII)
    after_pbc.extend_from_slice(&0xFFFFu16.to_le_bytes()); // SCREEN
                                                           // DMSG 1: control-data (bit 15), index 1, no text payload.
    after_pbc.extend_from_slice(&1u16.to_le_bytes());
    after_pbc.extend_from_slice(&0x8000u16.to_le_bytes());
    after_pbc.extend_from_slice(&0u16.to_le_bytes());
    // DMSG 2: display data, index 2, right tally red, label "SRC".
    after_pbc.extend_from_slice(&2u16.to_le_bytes());
    after_pbc.extend_from_slice(&0x0001u16.to_le_bytes());
    after_pbc.extend_from_slice(&3u16.to_le_bytes());
    after_pbc.extend_from_slice(b"SRC");
    let mut packet = (after_pbc.len() as u16).to_le_bytes().to_vec();
    packet.extend_from_slice(&after_pbc);

    let msg = tsl::v50::decode(&packet).expect("v5.0 mixed decode");
    assert_eq!(msg.displays.len(), 1);
    assert_eq!(msg.displays[0].index, 2);
    assert_eq!(msg.displays[0].text, "SRC");
    assert_eq!(msg.displays[0].right.color, TallyColor::Red);
}

#[test]
fn v50_rejects_all_control_data() {
    // Every DMSG skipped (or none at all) leaves no display to report.
    let mut after_pbc = Vec::new();
    after_pbc.push(0); // VER
    after_pbc.push(0); // FLAGS (ASCII)
    after_pbc.extend_from_slice(&0xFFFFu16.to_le_bytes()); // SCREEN
    after_pbc.extend_from_slice(&1u16.to_le_bytes()); // index 1
    after_pbc.extend_from_slice(&0x8000u16.to_le_bytes()); // control-data
    after_pbc.extend_from_slice(&0u16.to_le_bytes());
    let mut packet = (after_pbc.len() as u16).to_le_bytes().to_vec();
    packet.extend_from_slice(&after_pbc);

    let err = tsl::v50::decode(&packet).expect_err("no display DMSGs");
    assert!(matches!(err, TslError::Framing(_)), "got {err:?}");
}

#[test]
fn v50_rejects_pbc_mismatch() {
    let mut p = v50_golden_ascii();
    p[0] = p[0].wrapping_add(3); // lie about byte count
    assert!(matches!(tsl::v50::decode(&p), Err(TslError::Length { .. })));
}

#[test]
fn v50_rejects_text_running_past_end() {
    let mut p = v50_golden_ascii();
    // Inflate the last display's LENGTH field beyond the buffer. The LENGTH is
    // the 2 bytes right before the 3-byte "SRC" label, i.e. at len-5..len-3.
    let n = p.len();
    p[n - 5] = 0xFF;
    p[n - 4] = 0x00;
    // PBC will now mismatch first; trim PBC check by also bumping PBC so we reach
    // the per-display length path.
    let after = (n - 2) as u16; // unchanged PBC still equals available -> Length on text
    p[0..2].copy_from_slice(&after.to_le_bytes());
    assert!(matches!(tsl::v50::decode(&p), Err(TslError::Length { .. })));
}

#[test]
fn v50_rejects_oversize_packet() {
    let big = vec![0u8; tsl::v50::MAX_PACKET_LEN + 1];
    assert!(matches!(
        tsl::v50::decode(&big),
        Err(TslError::TooLong { .. })
    ));
}

#[test]
fn v50_stuffed_unstuffs_doubled_dle() {
    // A packet whose payload contains a literal DLE (0xFE) must survive
    // stuffing. The display index low byte 0xFE does it: index 0x00FE = 254.
    let label = b"x";
    let control: u16 = 0;
    let mut after_pbc = Vec::new();
    after_pbc.push(0);
    after_pbc.push(0);
    after_pbc.extend_from_slice(&0u16.to_le_bytes());
    after_pbc.extend_from_slice(&0x00FEu16.to_le_bytes()); // index 254 (0xFE low byte)
    after_pbc.extend_from_slice(&control.to_le_bytes());
    after_pbc.extend_from_slice(&(label.len() as u16).to_le_bytes());
    after_pbc.extend_from_slice(label);
    let mut raw = (after_pbc.len() as u16).to_le_bytes().to_vec();
    raw.extend_from_slice(&after_pbc);

    // Hand-stuff: DLE STX opener + payload with each DLE doubled. The v5.0
    // TCP framing has NO terminator — PBC gives the packet length.
    let mut stuffed = vec![tsl::v50::DLE, tsl::v50::STX];
    for &b in &raw {
        stuffed.push(b);
        if b == tsl::v50::DLE {
            stuffed.push(tsl::v50::DLE);
        }
    }
    assert_eq!(&stuffed[..2], &[0xFE, 0x02]);

    let msg = tsl::v50::decode_stuffed(&stuffed).expect("unstuffed decode");
    assert_eq!(msg.displays[0].index, 0xFE);
    assert_eq!(msg.displays[0].text, "x");
}

#[test]
fn v50_stuffed_rejects_dle_etx_terminator() {
    // A lone DLE followed by anything but DLE is not a valid escape. v5.0
    // has no DLE-ETX terminator, so `FE 03` mid-payload is a framing error.
    let p = v50_golden_ascii();
    let mut stuffed = vec![tsl::v50::DLE, tsl::v50::STX];
    for (i, &b) in p.iter().enumerate() {
        if i == 3 {
            stuffed.extend_from_slice(&[tsl::v50::DLE, 0x03]); // bad escape
            continue;
        }
        stuffed.push(b);
        if b == tsl::v50::DLE {
            stuffed.push(tsl::v50::DLE);
        }
    }
    assert!(matches!(
        tsl::v50::decode_stuffed(&stuffed),
        Err(TslError::Framing(_))
    ));
}
