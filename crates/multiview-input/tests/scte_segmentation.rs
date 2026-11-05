//! SCTE-35 segmentation descriptor regression vectors (pure Rust).
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]
use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::scte::{CueKind, SpliceInfoSection};

fn finish(mut body: Vec<u8>) -> Vec<u8> {
    let len = body.len() + 1;
    body[1] = 0x30 | ((len >> 8) as u8);
    body[2] = len as u8;
    body.extend_from_slice(&crc32_mpeg2(&body).to_be_bytes());
    body
}
fn descriptor(id: u32, kind: u8, cancel: bool) -> Vec<u8> {
    let mut data = b"CUEI".to_vec();
    data.extend_from_slice(&id.to_be_bytes());
    data.push(if cancel { 0xff } else { 0x7f });
    if !cancel {
        data.push(0xff); // program, duration, unrestricted
        data.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05]); // 40-bit duration
        data.extend_from_slice(&[0x0f, 3, 0, 0xfe, 0xff, kind, 1, 1]);
    }
    let mut out = vec![2, data.len() as u8];
    out.extend(data);
    out
}
fn section(descriptors: &[u8]) -> Vec<u8> {
    let mut body = vec![
        0xfc, 0x30, 0, 0, 0, 0, 0, 0, 7, 0, 0xff, 0xf0, 5, 6, 0xff, 0xff, 0xff, 0xff, 0xfd,
    ]; // pts = 2^33 - 3, adjustment = 7
    body.extend_from_slice(&(descriptors.len() as u16).to_be_bytes());
    body.extend_from_slice(descriptors);
    finish(body)
}
#[test]
fn segmentation_start_end_multiple_and_upid_are_preserved() {
    let mut descriptors = descriptor(42, 0x22, false);
    descriptors.extend(descriptor(43, 0x23, false));
    let bytes = section(&descriptors);
    let parsed = SpliceInfoSection::parse(&bytes).unwrap();
    let events = parsed.normalized_cue_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].cue.kind, CueKind::SpliceOut);
    assert_eq!(events[1].cue.kind, CueKind::SpliceIn);
    assert_eq!(events[0].cue.event_id, 42);
    assert_eq!(events[1].cue.event_id, 43);
    assert_eq!(events[0].cue.pts_time_90k, Some(4));
    assert_eq!(events[0].cue.break_duration_90k, Some(0x01_0203_0405));
    let desc = events[0].segmentation.as_ref().unwrap();
    assert_eq!(desc.segmentation_type_id, Some(0x22));
    let upid = desc.upid.as_ref().unwrap();
    assert_eq!(upid.upid_type, 0x0f);
    assert_eq!(upid.value, [0, 0xfe, 0xff]);
    assert_eq!(parsed.raw_section(), bytes);
    // Legacy callers continue receiving a bare time signal.
    assert_eq!(parsed.cue_event().unwrap().kind, CueKind::TimeSignal);
}
#[test]
fn segmentation_cancellation_has_identity_but_no_invented_type() {
    let parsed = SpliceInfoSection::parse(&section(&descriptor(99, 0, true))).unwrap();
    let events = parsed.normalized_cue_events();
    assert_eq!(events[0].cue.event_id, 99);
    assert!(events[0].cue.cancel);
    let desc = events[0].segmentation.as_ref().unwrap();
    assert_eq!(desc.segmentation_type_id, None);
    assert_eq!(desc.upid, None);
    assert_eq!(desc.duration_90k, None);
}
#[test]
fn descriptor_and_command_bounds_cannot_consume_crc_or_neighbors() {
    let valid = section(&descriptor(1, 0x22, false));
    for index in [12, 20, 22, 39] {
        // command, loop, descriptor, UPID lengths
        let mut body = valid[..valid.len() - 4].to_vec();
        body[index] = 0xff;
        assert!(
            SpliceInfoSection::parse(&finish(body)).is_err(),
            "index {index}"
        );
    }
    let mut bad_crc = valid.clone();
    *bad_crc.last_mut().unwrap() ^= 1;
    assert!(SpliceInfoSection::parse(&bad_crc).is_err());
    for len in 0..valid.len() {
        assert!(SpliceInfoSection::parse(&valid[..len]).is_err());
    }
}
#[test]
fn segmentation_components_and_delivery_restrictions_keep_field_alignment() {
    let mut desc = descriptor(21, 0x22, false);
    desc[11] = 0x40; // component mode, duration present, restricted delivery
    desc.splice(12..12, [1, 9, 0xfe, 0, 0, 0, 17]); // count, tag, offset
    desc[1] += 7;
    let parsed = SpliceInfoSection::parse(&section(&desc)).unwrap();
    assert_eq!(parsed.normalized_cue_events()[0].cue.event_id, 21);
    assert_eq!(
        parsed.normalized_cue_events()[0].cue.break_duration_90k,
        Some(0x01_0203_0405)
    );
    let rewritten = parsed.reserialize_with_pts_adjustment(9).unwrap();
    let reparsed = SpliceInfoSection::parse(&rewritten).unwrap();
    assert_eq!(
        reparsed.segmentation_descriptors,
        parsed.segmentation_descriptors
    );
    assert_eq!(
        reparsed.normalized_cue_events()[0].cue.pts_time_90k,
        Some(6)
    );
}

#[test]
fn descriptor_duration_is_optional_and_evidence_excludes_trailing_input() {
    let mut desc = descriptor(5, 0x23, false);
    desc[11] = 0xbf; // no duration
    desc.drain(12..17);
    desc[1] -= 5;
    let valid = section(&desc);
    let mut padded = valid.clone();
    padded.extend_from_slice(&[0xff; 200]);
    let parsed = SpliceInfoSection::parse(&padded).unwrap();
    assert_eq!(parsed.raw_section(), valid);
    assert_eq!(
        parsed.normalized_cue_events()[0].cue.break_duration_90k,
        None
    );
}

#[test]
fn unknown_descriptors_skip_and_unknown_segmentation_is_not_a_break() {
    let mut descriptors = vec![0x80, 2, 0xaa, 0xbb];
    descriptors.extend(descriptor(7, 0x10, false));
    let parsed = SpliceInfoSection::parse(&section(&descriptors)).unwrap();
    assert_eq!(
        parsed.normalized_cue_events()[0].cue.kind,
        CueKind::TimeSignal
    );
    assert_eq!(parsed.normalized_cue_events()[0].cue.event_id, 7);
}
