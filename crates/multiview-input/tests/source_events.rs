//! Pure SCTE source adapter regression tests.
#![allow(
    clippy::unwrap_used,
    clippy::assert_is_empty,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]
use multiview_core::{source_event::EventBoundary, time::Rational};
use multiview_input::{
    mpegts::crc::crc32_mpeg2,
    normalize::{PtsNormalizer, WrapBits},
    source_events::{AdapterLimits, ScteSourceEventAdapter},
};
fn finish(mut b: Vec<u8>) -> Vec<u8> {
    let n = b.len() + 1;
    b[1] = 0x30 | ((n >> 8) as u8);
    b[2] = n as u8;
    b.extend_from_slice(&crc32_mpeg2(&b).to_be_bytes());
    b
}
fn descriptor(id: u32, kind: u8, cancel: bool) -> Vec<u8> {
    let mut d = b"CUEI".to_vec();
    d.extend(id.to_be_bytes());
    d.push(if cancel { 0xff } else { 0x7f });
    if !cancel {
        d.extend([0xbf, 0x0f, 2, 0xab, 0xcd, kind, 1, 1]);
    }
    let mut b = vec![2, d.len() as u8];
    b.extend(d);
    b
}
fn section(ds: &[u8], pts: u64) -> Vec<u8> {
    let mut b = vec![
        0xfc,
        0x30,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0xff,
        0xf0,
        5,
        6,
        0xfe | ((pts >> 32) as u8),
    ];
    b.extend((pts as u32).to_be_bytes());
    b.extend((ds.len() as u16).to_be_bytes());
    b.extend(ds);
    finish(b)
}
fn adapter() -> ScteSourceEventAdapter {
    ScteSourceEventAdapter::new("s".into(), 7, AdapterLimits::default())
}
#[test]
fn unknown_then_anchor_revises_once_and_preserves_end() {
    let mut a = adapter();
    let start = section(&descriptor(0, 0x22, false), 90_000);
    let end = section(&descriptor(0, 0x23, false), 180_000);
    let s = a.ingest_section(&start, 1, None).unwrap();
    assert_eq!(
        s[0].boundary,
        EventBoundary::Start {
            pts_ns: None,
            duration_ns: None
        }
    );
    assert!(a.ingest_section(&start, 1, None).unwrap().is_empty());
    let e = a.ingest_section(&end, 1, None).unwrap();
    assert_eq!(s[0].key, e[0].key);
    assert_eq!(e[0].revision, 1);
    let mut n = PtsNormalizer::new(
        WrapBits::Mpeg33,
        Rational::new(1, 90_000),
        Rational::new(25, 1),
    );
    n.normalize(Some(0), 5_000_000_000).unwrap();
    let mapped = a.resolve_pending(n.mapping_snapshot().unwrap());
    assert_eq!(mapped.len(), 2);
    assert!(mapped.iter().all(|x| x.revision == 2));
    assert!(mapped.iter().any(|x| x.boundary
        == EventBoundary::End {
            pts_ns: Some(7_000_000_000)
        }));
    assert!(a.resolve_pending(n.mapping_snapshot().unwrap()).is_empty());
    assert!(a.ingest_section(&start, 1, None).unwrap().is_empty());
}
#[test]
fn occurrence_source_epoch_and_assets_do_not_merge() {
    let b = section(&descriptor(0, 0x22, false), 1);
    let mut a = adapter();
    let x = a.ingest_section(&b, 1, None).unwrap().remove(0);
    let y = a.ingest_section(&b, 2, None).unwrap().remove(0);
    assert_ne!(x.key, y.key);
    assert_eq!(x.asset_id, y.asset_id);
    assert_eq!(x.asset_id.as_deref(), Some("scte-upid:0f:abcd"));
    let mut other = ScteSourceEventAdapter::new("other".into(), 8, AdapterLimits::default());
    assert_ne!(x.key, other.ingest_section(&b, 1, None).unwrap()[0].key);
}
#[test]
fn unknown_generic_and_program_signals_are_not_breaks() {
    let mut a = adapter();
    for d in [
        vec![],
        descriptor(0, 0x10, false),
        descriptor(0, 0x30, false),
    ] {
        assert!(a
            .ingest_section(&section(&d, 0), 1, None)
            .unwrap()
            .is_empty());
    }
    assert_eq!(a.diagnostics().unsupported, 3);
    assert_eq!(a.retained_boundaries(), 0);
}
#[test]
fn bounded_retention_cancel_and_bad_crc() {
    let mut a = ScteSourceEventAdapter::new(
        "s".into(),
        0,
        AdapterLimits {
            max_boundaries: 2,
            max_source_bytes: 64,
        },
    );
    let b = section(&descriptor(9, 0x22, false), 1);
    a.ingest_section(&b, 1, None).unwrap();
    let c = a
        .ingest_section(&section(&descriptor(9, 0, true), 0), 1, None)
        .unwrap();
    assert_eq!(c[0].boundary, EventBoundary::Cancel);
    assert!(a.ingest_section(&b, 2, None).unwrap().is_empty());
    assert_eq!(a.diagnostics().capacity_exceeded, 1);
    let mut bad = b;
    *bad.last_mut().unwrap() ^= 1;
    assert!(a.ingest_section(&bad, 1, None).is_err());
    assert_eq!(a.retained_boundaries(), 2);
}
fn insert(id: u32) -> Vec<u8> {
    let mut b = vec![0xfc, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xf0, 15, 5];
    b.extend(id.to_be_bytes());
    b.extend([0x7f, 0xcf, 0xfe, 0, 0, 0, 1, 0, 0, 0, 0]);
    b.extend([0, 0]);
    finish(b)
}
#[test]
fn zero_insert_and_descriptor_ids_do_not_collide() {
    let mut a = adapter();
    let i = a.ingest_section(&insert(0), 0, None).unwrap();
    let s = a
        .ingest_section(&section(&descriptor(0, 0x22, false), 1), 0, None)
        .unwrap();
    assert_eq!(i.len(), 1);
    assert_ne!(i[0].key, s[0].key);
}
#[test]
fn early_end_and_nonsemantic_wire_changes() {
    let mut a = adapter();
    let end = a
        .ingest_section(&section(&descriptor(9, 0x23, false), 20), 0, None)
        .unwrap();
    let mut b = section(&descriptor(9, 0x22, false), 10);
    let start = a.ingest_section(&b, 0, None).unwrap();
    assert_eq!(end[0].key, start[0].key);
    assert_eq!(start[0].revision, 1);
    b.truncate(b.len() - 4);
    b[9] = 17;
    let b = finish(b);
    assert!(a.ingest_section(&b, 0, None).unwrap().is_empty());
}
#[test]
fn wrapped_mapping_exact_and_cancel_discards_pending() {
    let mut a = adapter();
    let mut n = PtsNormalizer::new(
        WrapBits::Mpeg33,
        Rational::new(1, 90_000),
        Rational::new(25, 1),
    );
    n.normalize(Some((1_i64 << 33) - 90_000), 1_000_000_000)
        .unwrap();
    let mapped = a
        .ingest_section(
            &section(&descriptor(1, 0x22, false), 0),
            0,
            n.mapping_snapshot(),
        )
        .unwrap();
    assert_eq!(
        mapped[0].boundary,
        EventBoundary::Start {
            pts_ns: Some(2_000_000_000),
            duration_ns: None
        }
    );
    a.ingest_section(&section(&descriptor(2, 0x22, false), 0), 0, None)
        .unwrap();
    a.ingest_section(&section(&descriptor(2, 0, true), 0), 0, None)
        .unwrap();
    assert!(a.resolve_pending(n.mapping_snapshot().unwrap()).is_empty());
}
#[test]
fn delayed_semantics_never_rewind_and_history_fails_closed() {
    let mut a = ScteSourceEventAdapter::new(
        "s".into(),
        0,
        AdapterLimits {
            max_boundaries: 2,
            max_source_bytes: 64,
        },
    );
    let d = descriptor(5, 0x22, false);
    assert_eq!(
        a.ingest_section(&section(&d, 1), 1, None).unwrap()[0].revision,
        1
    );
    assert_eq!(
        a.ingest_section(&section(&d, 2), 1, None).unwrap()[0].revision,
        2
    );
    assert!(a
        .ingest_section(&section(&d, 1), 1, None)
        .unwrap()
        .is_empty());
    assert!(a
        .ingest_section(&section(&d, 3), 1, None)
        .unwrap()
        .is_empty());
    assert!(a
        .ingest_section(&section(&d, 1), 1, None)
        .unwrap()
        .is_empty());
    assert_eq!(a.diagnostics().capacity_exceeded, 1);
    let mut n = PtsNormalizer::new(
        WrapBits::Mpeg33,
        Rational::new(1, 90_000),
        Rational::new(25, 1),
    );
    n.normalize(Some(2), 123).unwrap();
    let resolved = a.resolve_pending(n.mapping_snapshot().unwrap());
    assert_eq!(
        resolved[0].boundary,
        EventBoundary::Start {
            pts_ns: Some(123),
            duration_ns: None
        }
    );
    assert_eq!(resolved[0].revision, 3);
    assert!(a
        .ingest_section(&section(&d, 1), 1, None)
        .unwrap()
        .is_empty());
}
#[test]
fn mapped_replay_is_suppressed_without_eviction() {
    let mut a = adapter();
    let mut n = PtsNormalizer::new(
        WrapBits::Mpeg33,
        Rational::new(1, 90_000),
        Rational::new(25, 1),
    );
    n.normalize(Some(0), 0).unwrap();
    let d = descriptor(8, 0x22, false);
    let first = a
        .ingest_section(&section(&d, 90_000), 0, n.mapping_snapshot())
        .unwrap();
    let second = a
        .ingest_section(&section(&d, 180_000), 0, n.mapping_snapshot())
        .unwrap();
    assert_eq!(first[0].revision, 1);
    assert_eq!(second[0].revision, 2);
    assert!(a
        .ingest_section(&section(&d, 90_000), 0, n.mapping_snapshot())
        .unwrap()
        .is_empty());
    assert!(a.resolve_pending(n.mapping_snapshot().unwrap()).is_empty());
}
#[test]
fn revised_raw_timing_increments_only_its_boundary() {
    let mut a = adapter();
    let d = descriptor(5, 0x22, false);
    let x = a.ingest_section(&section(&d, 1), 1, None).unwrap();
    let y = a.ingest_section(&section(&d, 2), 1, None).unwrap();
    assert_eq!(x[0].revision, 1);
    assert_eq!(y[0].revision, 2);
    assert!(a
        .ingest_section(&section(&d, 2), 1, None)
        .unwrap()
        .is_empty());
}
