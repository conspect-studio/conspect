//! Golden on-wire vector tests for the SCTE-35 `splice_info_section` and the
//! SCTE-104 `multiple_operation_message` parsers, plus the normalised cue-event
//! projection. These run in the DEFAULT (pure-Rust) build — the parsers are
//! socket-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // reason: test fixtures hand-assemble byte vectors with small, statically
    // in-range length fields; `as` on those tiny constants cannot truncate.
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::scte::scte104::{Scte104Message, SpliceInsertType};
use multiview_input::scte::splice35::{SpliceCommand, SpliceInfoSection};
use multiview_input::scte::{CueKind, ScteError};

/// Append the correct CRC-32/MPEG-2 (the SCTE-35 section CRC) to a section body.
fn with_crc(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32_mpeg2(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    body
}

// ---------------------------------------------------------------------------
// SCTE-35 splice_info_section — a real-world splice_insert vector
// ---------------------------------------------------------------------------

/// The canonical SCTE-35 `splice_insert` body (out-of-network start with a PTS
/// and a break duration), as published in interoperability test suites, with the
/// trailing CRC-32/MPEG-2 recomputed over the body so the vector self-validates.
/// Body hex: FC302500000000000000FFF01405000000017FEFFE7369C02EFE0052CCF5000000000000
const SPLICE_INSERT_GOLDEN: &[u8] = &[
    0xFC, 0x30, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xF0, 0x14, 0x05, 0x00, 0x00,
    0x00, 0x01, 0x7F, 0xEF, 0xFE, 0x73, 0x69, 0xC0, 0x2E, 0xFE, 0x00, 0x52, 0xCC, 0xF5, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x26, 0x05, 0xE2, 0x37,
];

#[test]
fn scte35_parses_real_splice_insert() {
    let section = SpliceInfoSection::parse(SPLICE_INSERT_GOLDEN).expect("valid splice_insert");
    assert_eq!(section.command_type, 0x05);
    let SpliceCommand::Insert(insert) = section.command else {
        panic!("expected splice_insert, got {:?}", section.command);
    };
    assert_eq!(insert.splice_event_id, 1);
    assert!(insert.out_of_network);
    assert!(!insert.immediate);
    assert!(!insert.cancel);
    // The splice_time PTS and break_duration are both present in this vector.
    assert!(insert.pts_time_90k.is_some());
    assert!(insert.break_duration_90k.is_some());

    let cue = section.cue_event().expect("cue event");
    assert_eq!(cue.kind, CueKind::SpliceOut);
    assert_eq!(cue.event_id, 1);
    assert!(cue.pts_time_ns().is_some());
    assert!(cue.break_duration_ns().is_some());
}

#[test]
fn scte35_rejects_corrupt_crc() {
    let mut section = SPLICE_INSERT_GOLDEN.to_vec();
    let last = section.len() - 1;
    section[last] ^= 0xFF;
    assert!(matches!(
        SpliceInfoSection::parse(&section),
        Err(ScteError::Crc { .. })
    ));
}

#[test]
fn scte35_rejects_wrong_table_id() {
    let mut section = SPLICE_INSERT_GOLDEN.to_vec();
    section[0] = 0x00;
    assert!(matches!(
        SpliceInfoSection::parse(&section),
        Err(ScteError::Syntax(_))
    ));
}

/// Build a minimal `time_signal` `splice_info_section` with a specified PTS.
fn time_signal_section(pts_90k: u64) -> Vec<u8> {
    // After section_length: protocol_version(8)=0, encrypted(1)=0,
    // encryption_algorithm(6)=0, pts_adjustment(33)=0, cw_index(8)=0, tier(12),
    // splice_command_length(12), splice_command_type(8)=0x06, then splice_time.
    // splice_time: time_specified(1)=1, reserved(6), pts(33).
    let mut after = Vec::new();
    after.push(0x00); // protocol_version
    after.push(0x00); // encrypted(0) + enc_alg(0) + top bit of pts_adjustment
    after.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // rest of pts_adjustment (33 bits total)
    after.push(0x00); // cw_index
                      // tier(12) + splice_command_length(12): all zero except we set length 0xFFF.
    after.push(0x00); // tier hi
    after.push(0x0F); // tier lo (4) + scl hi (4)
    after.push(0xFF); // scl lo
    after.push(0x06); // splice_command_type = time_signal
                      // splice_time: 1 + 6 reserved + 33-bit pts. Pack into 5 bytes:
                      // byte0: 1 (time_specified) << 7 | 6 reserved bits | top bit of 33-bit pts.
    let pts = pts_90k & ((1u64 << 33) - 1);
    let b0 = 0x80 | (((pts >> 32) & 0x01) as u8);
    let b1 = ((pts >> 24) & 0xFF) as u8;
    let b2 = ((pts >> 16) & 0xFF) as u8;
    let b3 = ((pts >> 8) & 0xFF) as u8;
    let b4 = (pts & 0xFF) as u8;
    after.extend_from_slice(&[b0, b1, b2, b3, b4]);

    // Header: table_id(0xFC), ssi(1)+pi(0)+rsvd(2)+section_length(12).
    let section_length = after.len() + 4; // + CRC
    let mut out = Vec::new();
    out.push(0xFC);
    out.push(0b1000_0000 | 0b0011_0000 | ((section_length >> 8) as u8 & 0x0F));
    out.push((section_length & 0xFF) as u8);
    out.extend_from_slice(&after);
    with_crc(out)
}

#[test]
fn scte35_time_signal_carries_pts() {
    let section = time_signal_section(0x1_2345_6789);
    let parsed = SpliceInfoSection::parse(&section).expect("valid time_signal");
    let SpliceCommand::TimeSignal(ts) = parsed.command else {
        panic!("expected time_signal, got {:?}", parsed.command);
    };
    assert_eq!(ts.pts_time_90k, Some(0x1_2345_6789));
    let cue = parsed.cue_event().expect("cue");
    assert_eq!(cue.kind, CueKind::TimeSignal);
    assert_eq!(cue.pts_time_90k, Some(0x1_2345_6789));
}

#[test]
fn scte35_too_short_is_typed_error() {
    assert!(matches!(
        SpliceInfoSection::parse(&[0xFC, 0x30]),
        Err(ScteError::TooShort { .. })
    ));
}

// ---------------------------------------------------------------------------
// SCTE-104 multiple_operation_message
// ---------------------------------------------------------------------------

/// Build a SCTE-104 `multiple_operation_message` with one `splice_request_data`
/// op.
fn scte104_splice(insert_type: u8, event_id: u32, break_tenths: u16) -> Vec<u8> {
    // splice_request_data op body (14 bytes).
    let mut op = Vec::new();
    op.push(insert_type);
    op.extend_from_slice(&event_id.to_be_bytes());
    op.extend_from_slice(&0x0064u16.to_be_bytes()); // unique_program_id
    op.extend_from_slice(&0x0014u16.to_be_bytes()); // pre_roll 20ms
    op.extend_from_slice(&break_tenths.to_be_bytes()); // break_duration tenths
    op.push(0x00); // avail_num
    op.push(0x00); // avails_expected
    op.push(0x01); // auto_return_flag

    let mut msg = Vec::new();
    msg.extend_from_slice(&0xFFFFu16.to_be_bytes()); // reserved/opID = multi-op
    msg.extend_from_slice(&0u16.to_be_bytes()); // messageSize (unused here)
    msg.push(0x00); // protocol_version
    msg.push(0x00); // AS_index
    msg.push(0x00); // message_number
    msg.extend_from_slice(&0u16.to_be_bytes()); // DPI_PID_index
    msg.push(0x00); // SCTE35_protocol_version
    msg.push(0x00); // timestamp time_type = 0 (none)
    msg.push(0x01); // numOps = 1
                    // op: opID(2) = 0x0101, data_length(2), data.
    msg.extend_from_slice(&0x0101u16.to_be_bytes());
    msg.extend_from_slice(&(op.len() as u16).to_be_bytes());
    msg.extend_from_slice(&op);
    msg
}

#[test]
fn scte104_parses_splice_start() {
    let msg = scte104_splice(1, 42, 300); // spliceStart_normal, 30.0s break
    let parsed = Scte104Message::parse(&msg).expect("valid scte-104");
    assert_eq!(parsed.requests.len(), 1);
    let req = parsed.requests[0];
    assert_eq!(req.insert_type, SpliceInsertType::StartNormal);
    assert_eq!(req.event_id, 42);
    assert_eq!(req.break_duration_tenths, 300);
    assert!(req.auto_return);

    let cue = req.cue_event();
    assert_eq!(cue.kind, CueKind::SpliceOut);
    assert_eq!(cue.event_id, 42);
    // 300 tenths of a second = 30s = 2_700_000 ticks at 90 kHz.
    assert_eq!(cue.break_duration_90k, Some(2_700_000));
}

#[test]
fn scte104_splice_end_maps_to_splice_in() {
    let msg = scte104_splice(3, 7, 0); // spliceEnd_normal
    let parsed = Scte104Message::parse(&msg).expect("valid");
    let cue = parsed.cue_events()[0];
    assert_eq!(cue.kind, CueKind::SpliceIn);
    assert_eq!(cue.break_duration_90k, None);
}

#[test]
fn scte104_rejects_single_op_sentinel() {
    let mut msg = scte104_splice(1, 1, 0);
    msg[0] = 0x00;
    msg[1] = 0x05; // a single_operation_message opID, not the 0xFFFF multi sentinel
    assert!(matches!(
        Scte104Message::parse(&msg),
        Err(ScteError::Syntax(_))
    ));
}

#[test]
fn scte104_too_short_is_typed_error() {
    assert!(matches!(
        Scte104Message::parse(&[0xFF, 0xFF]),
        Err(ScteError::TooShort { .. })
    ));
}
