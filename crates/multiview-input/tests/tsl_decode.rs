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
// v4.0 — 19-byte core (address + control + 16 ASCII + checksum)
// ---------------------------------------------------------------------------

/// Build a v4.0 golden core: address 2, left=red, right=green, text=amber,
/// brightness full, label "VTR".
fn v40_golden() -> Vec<u8> {
    let mut p = vec![0u8; 19];
    p[0] = 0x80 | 0x02; // sync + address 2
                        // bits: LL=01(red) RR=10(green)<<2 TT=11(amber)<<4 BB=11<<6
    p[1] = 0b01 | (0b10 << 2) | (0b11 << 4) | (0b11 << 6);
    let label = b"VTR";
    p[2..2 + label.len()].copy_from_slice(label);
    for b in &mut p[2 + label.len()..18] {
        *b = b' ';
    }
    // checksum: 8-bit sum of bytes 0..18 plus checksum == 0
    let sum = p[..18].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    p[18] = sum.wrapping_neg();
    p
}

#[test]
fn v40_decodes_golden_vector() {
    let msg = tsl::v40::decode(&v40_golden()).expect("v4.0 decode");
    assert_eq!(msg.version, TslVersion::V40);
    assert_eq!(msg.displays.len(), 1);
    let d = &msg.displays[0];
    assert_eq!(d.index, 2);
    assert_eq!(d.text, "VTR");
    assert_eq!(d.left.color, TallyColor::Red);
    assert_eq!(d.right.color, TallyColor::Green);
    assert_eq!(d.text_tally.color, TallyColor::Amber);
    assert_eq!(d.left.brightness, Brightness::FULL);
}

#[test]
fn v40_rejects_bad_checksum() {
    let mut p = v40_golden();
    p[18] = p[18].wrapping_add(1); // corrupt checksum
    assert!(matches!(
        tsl::v40::decode(&p),
        Err(TslError::Checksum { .. })
    ));
}

#[test]
fn v40_framed_strips_dle_stx() {
    let mut framed = vec![tsl::v40::DLE, tsl::v40::STX];
    framed.extend_from_slice(&v40_golden());
    let msg = tsl::v40::decode_framed(&framed).expect("framed v4.0 decode");
    assert_eq!(msg.displays[0].text, "VTR");
}

#[test]
fn v40_framed_rejects_bad_opener() {
    let mut framed = vec![0x00, 0x00];
    framed.extend_from_slice(&v40_golden());
    assert!(matches!(
        tsl::v40::decode_framed(&framed),
        Err(TslError::Framing(_))
    ));
}

// ---------------------------------------------------------------------------
// v5.0 — variable-length IP packet
// ---------------------------------------------------------------------------

/// Build a v5.0 golden packet: screen 5, one display index 0, left=green,
/// right=red, text=off, brightness full, ASCII label "SRC".
fn v50_golden_ascii() -> Vec<u8> {
    let label = b"SRC";
    // control: L=10(green) R=01(red)<<2 T=00(off, omitted) B=11<<6
    let control: u16 = 0b10 | (0b01 << 2) | (0b11 << 6);
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
    assert_eq!(d.left.color, TallyColor::Green);
    assert_eq!(d.right.color, TallyColor::Red);
    assert_eq!(d.text_tally.color, TallyColor::Off);
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
    // A packet whose payload contains a literal 0x10 must survive DLE-stuffing.
    // Easiest: encode index 0x10 (low byte) somewhere. We craft a packet whose
    // first display index low byte is 0x10 so the raw bytes include 0x10.
    let label = b"x";
    let control: u16 = 0;
    let mut after_pbc = Vec::new();
    after_pbc.push(0);
    after_pbc.push(0);
    after_pbc.extend_from_slice(&0u16.to_le_bytes());
    after_pbc.extend_from_slice(&0x0010u16.to_le_bytes()); // index 16 (0x10 low byte)
    after_pbc.extend_from_slice(&control.to_le_bytes());
    after_pbc.extend_from_slice(&(label.len() as u16).to_le_bytes());
    after_pbc.extend_from_slice(label);
    let mut raw = (after_pbc.len() as u16).to_le_bytes().to_vec();
    raw.extend_from_slice(&after_pbc);

    // Hand-stuff: DLE STX + stuffed payload + DLE ETX.
    let mut stuffed = vec![tsl::v50::DLE, tsl::v50::STX];
    for &b in &raw {
        stuffed.push(b);
        if b == tsl::v50::DLE {
            stuffed.push(tsl::v50::DLE);
        }
    }
    stuffed.push(tsl::v50::DLE);
    stuffed.push(0x03); // ETX

    let msg = tsl::v50::decode_stuffed(&stuffed).expect("unstuffed decode");
    assert_eq!(msg.displays[0].index, 16);
    assert_eq!(msg.displays[0].text, "x");
}
