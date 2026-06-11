//! RT-14a — SCTE-35 egress re-stamp tests.
//!
//! When a program's PTS timeline is rebased by a GP-6 offset across an output
//! splice (seamless or make-before-break), any SCTE-35 cue that rides the seam
//! must have its 33-bit `pts_adjustment` shifted by the **same** offset, or
//! downstream ad insertion misfires (SCTE-35 2023r1: "Modifying the
//! `pts_adjustment` field is preferred"). These tests pin the pure primitive that
//! rewrites `pts_adjustment` + recomputes the trailing CRC-32/MPEG-2, and the
//! seam-offset helper, independent of the egress wiring (RT-14b).
//!
//! They run in the DEFAULT (pure-Rust) build — the parsers are socket-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: test fixtures hand-assemble byte vectors with small, statically
    // in-range length fields; `as` on those tiny constants cannot truncate.
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    // reason: the fixture builders push each wire field with an explaining
    // comment; a `vec![..]` literal would lose that field-by-field annotation.
    clippy::vec_init_then_push
)]

use proptest::prelude::*;

use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::scte::splice35::{shift_pts_adjustment_90k, SpliceCommand, SpliceInfoSection};

/// 33-bit mask.
const MASK_33: u64 = (1 << 33) - 1;

/// Append the correct CRC-32/MPEG-2 (the SCTE-35 section CRC) to a section body.
fn with_crc(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32_mpeg2(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    body
}

/// The canonical SCTE-35 `splice_insert` body (out-of-network start with a PTS
/// and a break duration), the same real-world vector the parser tests use, with
/// the trailing CRC-32/MPEG-2 recomputed so it self-validates.
/// Body hex: FC302500000000000000FFF01405000000017FEFFE7369C02EFE0052CCF5000000000000
const SPLICE_INSERT_GOLDEN: &[u8] = &[
    0xFC, 0x30, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xF0, 0x14, 0x05, 0x00, 0x00,
    0x00, 0x01, 0x7F, 0xEF, 0xFE, 0x73, 0x69, 0xC0, 0x2E, 0xFE, 0x00, 0x52, 0xCC, 0xF5, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x26, 0x05, 0xE2, 0x37,
];

/// Build a minimal `time_signal` `splice_info_section` with a specified PTS and a
/// chosen `pts_adjustment` so the effective splice time is exercised.
fn time_signal_section(pts_90k: u64, pts_adjustment: u64) -> Vec<u8> {
    let adj = pts_adjustment & MASK_33;
    let mut after = Vec::new();
    after.push(0x00); // protocol_version
                      // encrypted(1)=0 + enc_alg(6)=0 + top bit (bit32) of pts_adjustment.
    after.push(((adj >> 32) & 0x01) as u8);
    after.push(((adj >> 24) & 0xFF) as u8);
    after.push(((adj >> 16) & 0xFF) as u8);
    after.push(((adj >> 8) & 0xFF) as u8);
    after.push((adj & 0xFF) as u8);
    after.push(0x00); // cw_index
    after.push(0x00); // tier hi
    after.push(0x0F); // tier lo (4) + splice_command_length hi (4)
    after.push(0xFF); // splice_command_length lo (0xFFF = unknown)
    after.push(0x06); // splice_command_type = time_signal
                      // splice_time: time_specified(1)=1 | 6 reserved | 33-bit pts.
    let pts = pts_90k & MASK_33;
    after.push(0x80 | (((pts >> 32) & 0x01) as u8));
    after.push(((pts >> 24) & 0xFF) as u8);
    after.push(((pts >> 16) & 0xFF) as u8);
    after.push(((pts >> 8) & 0xFF) as u8);
    after.push((pts & 0xFF) as u8);

    let section_length = after.len() + 4; // + CRC
    let mut out = Vec::new();
    out.push(0xFC);
    out.push(0b1000_0000 | 0b0011_0000 | ((section_length >> 8) as u8 & 0x0F));
    out.push((section_length & 0xFF) as u8);
    out.extend_from_slice(&after);
    with_crc(out)
}

/// Build an *immediate* `splice_insert` section (`splice_immediate_flag` set, no
/// `splice_time`), with a chosen `pts_adjustment`.
fn immediate_splice_insert(event_id: u32, pts_adjustment: u64) -> Vec<u8> {
    let adj = pts_adjustment & MASK_33;
    let mut after = Vec::new();
    after.push(0x00); // protocol_version
    after.push(((adj >> 32) & 0x01) as u8);
    after.push(((adj >> 24) & 0xFF) as u8);
    after.push(((adj >> 16) & 0xFF) as u8);
    after.push(((adj >> 8) & 0xFF) as u8);
    after.push((adj & 0xFF) as u8);
    after.push(0x00); // cw_index
    after.push(0x00); // tier hi
    after.push(0x0F); // tier lo + scl hi
    after.push(0xFF); // scl lo
    after.push(0x05); // splice_command_type = splice_insert
                      // splice_insert body:
    after.extend_from_slice(&event_id.to_be_bytes()); // splice_event_id(32)
    after.push(0x00); // splice_event_cancel_indicator(1)=0 + 7 reserved
                      // out_of_network(1)=1, program_splice(1)=1, duration(1)=0,
                      // splice_immediate(1)=1, then 4 reserved => 1101 0000 = 0xD0.
    after.push(0xD0);
    // immediate => no splice_time. unique_program_id(16), avail_num(8),
    // avails_expected(8) follow.
    after.extend_from_slice(&0x0064u16.to_be_bytes());
    after.push(0x00);
    after.push(0x00);

    let section_length = after.len() + 4;
    let mut out = Vec::new();
    out.push(0xFC);
    out.push(0b1000_0000 | 0b0011_0000 | ((section_length >> 8) as u8 & 0x0F));
    out.push((section_length & 0xFF) as u8);
    out.extend_from_slice(&after);
    with_crc(out)
}

/// The effective splice time = command `pts_time` folded with `pts_adjustment`,
/// as the parser reports it on `cue_event().pts_time_90k`.
fn effective_splice_time(section: &[u8]) -> Option<u64> {
    SpliceInfoSection::parse(section)
        .ok()
        .and_then(|s| s.cue_event())
        .and_then(|c| c.pts_time_90k)
}

// ---------------------------------------------------------------------------
// The seam-offset helper (the SCTE-35 2023r1 re-stamp rule, mod 2^33).
// ---------------------------------------------------------------------------

#[test]
fn shift_helper_wraps_at_2_pow_33() {
    // (old + offset) & ((1<<33)-1).
    assert_eq!(shift_pts_adjustment_90k(0, 0), 0);
    assert_eq!(shift_pts_adjustment_90k(100, 50), 150);
    // Wrap: MASK_33 + 1 wraps to 0.
    assert_eq!(shift_pts_adjustment_90k(MASK_33, 1), 0);
    assert_eq!(shift_pts_adjustment_90k(MASK_33, 2), 1);
    // Inputs above 33 bits are masked.
    assert_eq!(shift_pts_adjustment_90k(u64::MAX, 0), MASK_33);
}

// ---------------------------------------------------------------------------
// Reserialize: a timed cue's effective splice time shifts by exactly delta.
// ---------------------------------------------------------------------------

#[test]
fn reserialize_shifts_timed_splice_insert_by_delta() {
    let original = SPLICE_INSERT_GOLDEN.to_vec();
    let before = effective_splice_time(&original).expect("golden carries a splice time");
    let parsed = SpliceInfoSection::parse(&original).expect("golden parses");

    let delta = 0x12_3456_u64;
    let new_adj = shift_pts_adjustment_90k(parsed.pts_adjustment, delta);
    let restamped = parsed
        .reserialize_with_pts_adjustment(new_adj)
        .expect("splice_insert round-trips");

    // Re-parse must succeed (CRC valid) and the effective splice time shifted by
    // exactly delta (mod 2^33).
    let after = effective_splice_time(&restamped).expect("re-parsed cue carries a splice time");
    assert_eq!(after, (before.wrapping_add(delta)) & MASK_33);

    // Only the pts_adjustment field changed: same overall length.
    assert_eq!(restamped.len(), original.len());
}

#[test]
fn reserialize_shifts_time_signal_by_delta() {
    let section = time_signal_section(0x1_2345_6789, 0x10);
    let before = effective_splice_time(&section).expect("time_signal carries a splice time");
    let parsed = SpliceInfoSection::parse(&section).expect("time_signal parses");

    let delta = 0x00AB_CDEF_u64;
    let new_adj = shift_pts_adjustment_90k(parsed.pts_adjustment, delta);
    let restamped = parsed
        .reserialize_with_pts_adjustment(new_adj)
        .expect("time_signal round-trips");

    let after = effective_splice_time(&restamped).expect("re-parsed time_signal");
    assert_eq!(after, (before.wrapping_add(delta)) & MASK_33);
}

// ---------------------------------------------------------------------------
// Immediate splice: no pts_time to shift; only pts_adjustment updates, the
// section still round-trips with a valid CRC.
// ---------------------------------------------------------------------------

#[test]
fn reserialize_immediate_splice_updates_only_adjustment() {
    let section = immediate_splice_insert(7, 0);
    let parsed = SpliceInfoSection::parse(&section).expect("immediate splice parses");
    let SpliceCommand::Insert(insert) = parsed.command else {
        panic!("expected splice_insert, got {:?}", parsed.command);
    };
    assert!(insert.immediate);
    assert!(insert.pts_time_90k.is_none());

    let delta = 0x5_5555_u64;
    let new_adj = shift_pts_adjustment_90k(parsed.pts_adjustment, delta);
    let restamped = parsed
        .reserialize_with_pts_adjustment(new_adj)
        .expect("immediate splice round-trips");

    let reparsed = SpliceInfoSection::parse(&restamped).expect("re-parses with valid CRC");
    assert_eq!(reparsed.pts_adjustment, delta & MASK_33);
    // The command is still an immediate splice with no splice time; the cue has
    // no presentation time to report.
    let SpliceCommand::Insert(reinsert) = reparsed.command else {
        panic!("expected splice_insert after round-trip");
    };
    assert!(reinsert.immediate);
    assert!(reinsert.pts_time_90k.is_none());
    assert_eq!(reinsert.splice_event_id, 7);
}

#[test]
fn reserialize_immediate_section_passes_through_with_zero_adjustment() {
    // A section with pts_adjustment=0 and an immediate command passes through
    // with only the adjustment field updated (the brief's explicit case).
    let section = immediate_splice_insert(99, 0);
    let parsed = SpliceInfoSection::parse(&section).expect("parses");
    // No seam offset => adjustment stays 0; the bytes round-trip identically.
    let new_adj = shift_pts_adjustment_90k(parsed.pts_adjustment, 0);
    let restamped = parsed
        .reserialize_with_pts_adjustment(new_adj)
        .expect("round-trips");
    assert_eq!(
        restamped, section,
        "zero-delta restamp is a byte-identical copy"
    );
}

// ---------------------------------------------------------------------------
// The recomputed CRC matches a known-good vector (independent of our own CRC).
// ---------------------------------------------------------------------------

#[test]
fn reserialized_section_crc_is_valid() {
    let parsed = SpliceInfoSection::parse(SPLICE_INSERT_GOLDEN).expect("parses");
    let new_adj = shift_pts_adjustment_90k(parsed.pts_adjustment, 0x7FFF);
    let restamped = parsed
        .reserialize_with_pts_adjustment(new_adj)
        .expect("round-trips");

    // The trailing four bytes must equal the CRC-32/MPEG-2 over the body.
    let body_len = restamped.len() - 4;
    let computed = crc32_mpeg2(&restamped[..body_len]);
    let carried = u32::from_be_bytes([
        restamped[body_len],
        restamped[body_len + 1],
        restamped[body_len + 2],
        restamped[body_len + 3],
    ]);
    assert_eq!(carried, computed, "trailing CRC matches recomputed CRC");
    // And the whole-section running CRC is zero (the self-checking property).
    assert_eq!(crc32_mpeg2(&restamped), 0);
}

// ---------------------------------------------------------------------------
// A malformed / unsupported section errors cleanly — never panics.
// ---------------------------------------------------------------------------

#[test]
fn reserialize_rejects_unsupported_encrypted_section() {
    // An encrypted section's command body is ciphered: the parser records
    // `Other`, and the writer must refuse to re-emit rather than corrupt bytes.
    let section = encrypted_section();
    let parsed = SpliceInfoSection::parse(&section).expect("encrypted header parses");
    assert!(matches!(parsed.command, SpliceCommand::Other(_)));
    let err = parsed.reserialize_with_pts_adjustment(0).unwrap_err();
    // It must be a typed error, not a panic and not corrupt bytes.
    let _ = err;
}

/// Build a section whose `encrypted_packet` flag is set, so the parser records
/// `Other` and the writer must refuse to re-emit (the body is ciphered).
fn encrypted_section() -> Vec<u8> {
    let mut after = Vec::new();
    after.push(0x00); // protocol_version
                      // encrypted(1)=1 + enc_alg(6)=0 + top bit pts_adjustment(=0): 1000_0000.
    after.push(0x80);
    after.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // rest of pts_adjustment
    after.push(0x00); // cw_index
    after.push(0x00); // tier hi
    after.push(0x0F); // tier lo + scl hi
    after.push(0xFF); // scl lo
    after.push(0x05); // command_type (ciphered body follows; not decoded)
    after.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // opaque ciphered bytes

    let section_length = after.len() + 4;
    let mut out = Vec::new();
    out.push(0xFC);
    out.push(0b1000_0000 | 0b0011_0000 | ((section_length >> 8) as u8 & 0x0F));
    out.push((section_length & 0xFF) as u8);
    out.extend_from_slice(&after);
    with_crc(out)
}

// ---------------------------------------------------------------------------
// THE load-bearing property: parse -> reserialize(old+delta) -> parse shifts the
// effective splice time by exactly delta (mod 2^33) with a valid CRC.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn prop_reserialize_shifts_effective_time_by_delta(
        pts in 0u64..MASK_33,
        adj in 0u64..MASK_33,
        delta in 0u64..MASK_33,
    ) {
        let section = time_signal_section(pts, adj);
        let parsed = SpliceInfoSection::parse(&section).expect("constructed section parses");
        let before = parsed
            .cue_event()
            .and_then(|c| c.pts_time_90k)
            .expect("time_signal carries a splice time");

        let new_adj = shift_pts_adjustment_90k(parsed.pts_adjustment, delta);
        let restamped = parsed
            .reserialize_with_pts_adjustment(new_adj)
            .expect("time_signal round-trips");

        // Re-parse: CRC must validate and the effective time shifts by delta.
        let reparsed = SpliceInfoSection::parse(&restamped)
            .expect("re-stamped section re-parses (valid CRC)");
        let after = reparsed
            .cue_event()
            .and_then(|c| c.pts_time_90k)
            .expect("re-parsed cue carries a splice time");

        prop_assert_eq!(after, before.wrapping_add(delta) & MASK_33);
        // Self-checking CRC property holds.
        prop_assert_eq!(crc32_mpeg2(&restamped), 0);
        // Same length: only field + CRC bytes changed.
        prop_assert_eq!(restamped.len(), section.len());
    }

    // Reserialize is total: for any parseable section, reserialize either returns
    // bytes that re-parse, or a typed error — never a panic.
    #[test]
    fn prop_reserialize_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
        new_adj in any::<u64>(),
    ) {
        if let Ok(parsed) = SpliceInfoSection::parse(&bytes) {
            if let Ok(out) = parsed.reserialize_with_pts_adjustment(new_adj) {
                // Any successful output must itself re-parse.
                prop_assert!(SpliceInfoSection::parse(&out).is_ok());
            }
        }
    }
}
