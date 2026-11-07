//! Marker identity text: the SCTE-35 `segmentation_type_id` name table (every
//! listed id, plus unknown), `splice_insert` ad-break naming, UPID suffixes,
//! segment ordinals, and timed-ID3 frame naming.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty
)]
use multiview_core::source_event::EventBoundary;
use multiview_input::mpegts::crc::crc32_mpeg2;
use multiview_input::scte::{segmentation_type_name, SpliceInfoSection};
use multiview_input::source_events::{AdapterLimits, ScteSourceEventAdapter};

/// The canonical SCTE-35 `splice_insert` golden vector (out-of-network start
/// with a PTS and a 10s break duration), also used in `scte_cue.rs`.
const SPLICE_INSERT_GOLDEN: &[u8] = &[
    0xFC, 0x30, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xF0, 0x14, 0x05, 0x00, 0x00,
    0x00, 0x01, 0x7F, 0xEF, 0xFE, 0x73, 0x69, 0xC0, 0x2E, 0xFE, 0x00, 0x52, 0xCC, 0xF5, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x26, 0x05, 0xE2, 0x37,
];

fn finish(mut b: Vec<u8>) -> Vec<u8> {
    let n = b.len() + 1;
    b[1] = 0x30 | ((n >> 8) as u8);
    b[2] = n as u8;
    b.extend_from_slice(&crc32_mpeg2(&b).to_be_bytes());
    b
}
/// A segmentation descriptor for `kind`, with an optional URI UPID and
/// segment ordinal so tests can exercise the detail-suffix paths.
fn descriptor(id: u32, kind: u8, upid: Option<&str>, segment: Option<(u8, u8)>) -> Vec<u8> {
    let mut d = b"CUEI".to_vec();
    d.extend(id.to_be_bytes());
    d.push(0x7f); // not cancelled
    d.push(0xbf); // program-level, no duration, unrestricted
    match upid {
        Some(text) => {
            d.push(0x0f); // URI
            d.push(u8::try_from(text.len()).unwrap());
            d.extend(text.as_bytes());
        }
        None => d.extend([0x00, 0]), // not-used type, zero-length UPID
    }
    d.push(kind);
    let (num, expected) = segment.unwrap_or((0, 0));
    d.extend([num, expected]);
    let mut b = vec![2, u8::try_from(d.len()).unwrap()];
    b.extend(d);
    b
}
fn section(ds: &[u8]) -> Vec<u8> {
    let mut b = vec![
        0xfc, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xf0, 5, 6, 0xfe, 0, 1, 0x86, 0xa0,
    ]; // time_signal, pts = 100_000
    b.extend((ds.len() as u16).to_be_bytes());
    b.extend(ds);
    finish(b)
}
fn adapter() -> ScteSourceEventAdapter {
    ScteSourceEventAdapter::new("s".into(), 1, AdapterLimits::default())
}

/// Every standard SCTE-35 2023r1 Table 23 `segmentation_type_id` this
/// adapter names, plus a reserved id that must stay unnamed.
#[test]
fn segmentation_type_table_matches_every_standard_name() {
    let expected: &[(u8, &str)] = &[
        (0x10, "Program Start"),
        (0x11, "Program End"),
        (0x12, "Program Early Termination"),
        (0x13, "Program Breakaway"),
        (0x14, "Program Resumption"),
        (0x17, "Program Overlap Start"),
        (0x19, "Program Blackout Override"),
        (0x20, "Chapter Start"),
        (0x21, "Chapter End"),
        (0x22, "Break Start"),
        (0x23, "Break End"),
        (0x24, "Opening Credit Start"),
        (0x25, "Opening Credit End"),
        (0x26, "Closing Credit Start"),
        (0x27, "Closing Credit End"),
        (0x30, "Provider Advertisement Start"),
        (0x31, "Provider Advertisement End"),
        (0x32, "Distributor Advertisement Start"),
        (0x33, "Distributor Advertisement End"),
        (0x34, "Provider Placement Opportunity Start"),
        (0x35, "Provider Placement Opportunity End"),
        (0x36, "Distributor Placement Opportunity Start"),
        (0x37, "Distributor Placement Opportunity End"),
        (0x38, "Provider Overlay Placement Opportunity Start"),
        (0x39, "Provider Overlay Placement Opportunity End"),
        (0x3A, "Distributor Overlay Placement Opportunity Start"),
        (0x3B, "Distributor Overlay Placement Opportunity End"),
        (0x3C, "Provider Promo Start"),
        (0x3D, "Provider Promo End"),
        (0x3E, "Distributor Promo Start"),
        (0x3F, "Distributor Promo End"),
        (0x40, "Unscheduled Event Start"),
        (0x41, "Unscheduled Event End"),
        (0x42, "Alternate Content Opportunity Start"),
        (0x43, "Alternate Content Opportunity End"),
        (0x44, "Provider Ad Block Start"),
        (0x45, "Provider Ad Block End"),
        (0x46, "Distributor Ad Block Start"),
        (0x47, "Distributor Ad Block End"),
        (0x50, "Network Start"),
        (0x51, "Network End"),
    ];
    for &(id, name) in expected {
        assert_eq!(segmentation_type_name(id), Some(name), "id 0x{id:02X}");
    }
    // A reserved/unassigned id must stay honestly unnamed, never guessed.
    assert_eq!(segmentation_type_name(0x15), None);
    assert_eq!(segmentation_type_name(0x60), None);
}

// The timed adapter only tracks the two actionable segmentation edges it
// documents (Break Start 0x22 / Break End 0x23); other segmentation types are
// presence-only evidence handled by the untimed-SCTE path instead (see
// `multiview-cli::source_event_runtime::NativeEvents::observe_scte_untimed`,
// which names any segmentation type via the same `segmentation_type_name`
// table). These tests exercise descriptor naming end-to-end on the edges this
// adapter actually emits.

#[test]
fn break_start_descriptor_names_the_marker_with_no_detail() {
    let mut a = adapter();
    let bytes = section(&descriptor(1, 0x22, None, None));
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    assert_eq!(obs.len(), 1);
    let d = obs[0].descriptor.as_ref().expect("descriptor");
    assert_eq!(d.name, "Break Start");
    assert_eq!(d.type_id.as_deref(), Some("0x22"));
    assert_eq!(d.detail, None);
}

#[test]
fn break_end_descriptor_is_named_too() {
    let mut a = adapter();
    let bytes = section(&descriptor(1, 0x23, None, None));
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    let d = obs[0].descriptor.as_ref().expect("descriptor");
    assert_eq!(d.name, "Break End");
    assert_eq!(d.type_id.as_deref(), Some("0x23"));
}

#[test]
fn break_start_with_segment_ordinal_gets_a_plain_suffix_detail() {
    let mut a = adapter();
    let bytes = section(&descriptor(2, 0x22, None, Some((2, 4))));
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    let d = obs[0].descriptor.as_ref().expect("descriptor");
    assert_eq!(d.name, "Break Start");
    assert_eq!(d.detail.as_deref(), Some("2/4"));
}

#[test]
fn readable_upid_contributes_a_bounded_suffix() {
    let mut a = adapter();
    let bytes = section(&descriptor(3, 0x22, Some("ad-id-123"), None));
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    let d = obs[0].descriptor.as_ref().expect("descriptor");
    assert_eq!(d.name, "Break Start");
    assert_eq!(d.detail.as_deref(), Some("URI:ad-id-123"));
}

#[test]
fn unassigned_segmentation_type_is_rejected_before_naming_is_reached() {
    // 0x15 is not an actionable edge in this adapter at all (not one of the
    // two supported break boundaries), so it is dropped as unsupported —
    // never silently mis-named.
    let mut a = adapter();
    let bytes = section(&descriptor(4, 0x15, None, None));
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    assert!(obs.is_empty());
    assert_eq!(a.diagnostics().unsupported, 1);
}

#[test]
fn splice_insert_out_of_network_names_an_ad_break_with_duration_detail() {
    let mut a = adapter();
    let obs = a.ingest_section(SPLICE_INSERT_GOLDEN, 0, None).unwrap();
    assert_eq!(obs.len(), 1);
    let d = obs[0].descriptor.as_ref().expect("descriptor");
    assert_eq!(d.name, "Ad break");
    assert_eq!(d.type_id.as_deref(), Some("splice_insert"));
    // Golden vector break_duration_90k: 0x00_52CCF5 ticks @ 90kHz.
    let section = SpliceInfoSection::parse(SPLICE_INSERT_GOLDEN).unwrap();
    let want_seconds = section.cue_event().unwrap().break_duration_ns().unwrap() / 1_000_000_000;
    assert_eq!(
        d.detail.as_deref(),
        Some(format!("{want_seconds}s").as_str())
    );
}

#[test]
fn splice_insert_return_names_return_from_break() {
    let mut a = adapter();
    // Flip out_of_network_indicator (top bit of byte 19) to make this a splice-in.
    let mut bytes = SPLICE_INSERT_GOLDEN.to_vec();
    bytes[19] &= 0x7f;
    let crc_start = bytes.len() - 4;
    let crc = crc32_mpeg2(&bytes[..crc_start]);
    bytes[crc_start..].copy_from_slice(&crc.to_be_bytes());
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    let d = obs[0].descriptor.as_ref().expect("descriptor");
    assert_eq!(d.name, "Return from break");
    assert_eq!(d.type_id.as_deref(), Some("splice_insert"));
    assert_eq!(d.detail, None);
}

#[test]
fn cancellation_carries_no_descriptor() {
    let mut a = adapter();
    let mut d = b"CUEI".to_vec();
    d.extend(5_u32.to_be_bytes());
    d.push(0xff); // cancel
    let mut cancelled = vec![2, u8::try_from(d.len()).unwrap()];
    cancelled.extend(d);
    let bytes = section(&cancelled);
    let obs = a.ingest_section(&bytes, 0, None).unwrap();
    assert!(matches!(obs[0].boundary, EventBoundary::Cancel));
    assert_eq!(obs[0].descriptor, None);
}
